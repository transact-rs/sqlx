//! Adapters that bridge `AsyncRead + AsyncWrite` implementations into sqlx's internal [`Socket`] trait.
//!
//! These adapters exist so users can pass pre-connected streams (vsock, QUIC, turmoil, etc.)
//! to sqlx without exposing the `Socket` trait as public API.
//!
//! ## Design notes
//!
//! The [`Socket`] trait uses a split-phase read model: `poll_read_ready` signals data is available,
//! then `try_read` synchronously copies from an internal buffer. Since `AsyncRead` doesn't have a
//! separate readiness notification, `poll_read_ready` performs the actual read into an internal
//! buffer, and `try_read` drains from it.
//!
//! `try_write` uses a noop waker to attempt a non-blocking poll_write. This is safe because
//! the caller (`Write` future in `socket/mod.rs`) always calls `poll_write_ready(cx)` with the
//! real task waker when `try_write` returns `WouldBlock`, ensuring proper wakeup registration.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BufMut;

use crate::io::ReadBuf;
use crate::net::Socket;

/// Internal buffer size for the read-ahead used by `poll_read_ready`.
const ADAPTER_BUF_SIZE: usize = 8192;

/// Generates an adapter struct + `Socket` impl for a given async I/O trait family.
macro_rules! impl_socket_adapter {
    (
        $(#[$meta:meta])*
        $name:ident,
        feature = $feature:literal,
        bounds($($bound:path),+),
        poll_read($self:ident, $cx:ident, $buf:ident) => $poll_read_expr:expr,
        poll_write($self_w:ident, $cx_w:ident, $buf_w:ident) => $poll_write_expr:expr,
        poll_flush($self_f:ident, $cx_f:ident) => $poll_flush_expr:expr,
        poll_shutdown($self_s:ident, $cx_s:ident) => $poll_shutdown_expr:expr $(,)?
    ) => {
        $(#[$meta])*
        #[cfg(feature = $feature)]
        pub struct $name<S> {
            inner: S,
            read_buf: Vec<u8>,
            read_len: usize,
            read_pos: usize,
        }

        #[cfg(feature = $feature)]
        impl<S> $name<S> {
            pub fn new(inner: S) -> Self {
                Self {
                    inner,
                    read_buf: vec![0u8; ADAPTER_BUF_SIZE],
                    read_len: 0,
                    read_pos: 0,
                }
            }

            fn buffered(&self) -> &[u8] {
                &self.read_buf[self.read_pos..self.read_len]
            }
        }

        #[cfg(feature = $feature)]
        impl<S> Socket for $name<S>
        where
            S: $($bound +)+ Send + Sync + Unpin + 'static,
        {
            fn try_read(&mut self, buf: &mut dyn ReadBuf) -> io::Result<usize> {
                let buffered = self.buffered();
                if !buffered.is_empty() {
                    let to_copy = std::cmp::min(buffered.len(), buf.remaining_mut());
                    buf.put_slice(&buffered[..to_copy]);
                    self.read_pos += to_copy;
                    if self.read_pos == self.read_len {
                        self.read_pos = 0;
                        self.read_len = 0;
                    }
                    return Ok(to_copy);
                }
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }

            fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
                // Safe to use noop_waker here: if Pending is returned, the caller
                // (Socket::write future) will call poll_write_ready(cx) with the real
                // task context, which re-registers the proper waker.
                let waker = futures_util::task::noop_waker();
                let mut cx = Context::from_waker(&waker);
                let $self_w = &mut self.inner;
                let $buf_w = buf;
                let $cx_w = &mut cx;
                match $poll_write_expr {
                    Poll::Ready(result) => result,
                    Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
                }
            }

            fn poll_read_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                if !self.buffered().is_empty() {
                    return Poll::Ready(Ok(()));
                }

                self.read_pos = 0;
                self.read_len = 0;

                let $cx = cx;
                let $self = &mut self.inner;
                let $buf = &mut self.read_buf;
                match $poll_read_expr {
                    Poll::Ready(Ok(n)) => {
                        if n == 0 {
                            return Poll::Ready(Err(io::Error::from(
                                io::ErrorKind::UnexpectedEof,
                            )));
                        }
                        self.read_len = n;
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                    Poll::Pending => Poll::Pending,
                }
            }

            fn poll_write_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                // Attempt a zero-byte write to check if the underlying stream is writable.
                // This registers the real waker with the I/O resource so we get woken
                // when the socket becomes writable.
                let $cx_w = cx;
                let $self_w = &mut self.inner;
                let $buf_w: &[u8] = &[];
                match $poll_write_expr {
                    Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                    Poll::Pending => Poll::Pending,
                }
            }

            fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                let $cx_f = cx;
                let $self_f = &mut self.inner;
                $poll_flush_expr
            }

            fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                let $cx_s = cx;
                let $self_s = &mut self.inner;
                $poll_shutdown_expr
            }
        }
    };
}

impl_socket_adapter! {
    /// Adapter that wraps a tokio [`AsyncRead`][tokio::io::AsyncRead] +
    /// [`AsyncWrite`][tokio::io::AsyncWrite] into a [`Socket`] implementation.
    TokioStream,
    feature = "_rt-tokio",
    bounds(tokio::io::AsyncRead, tokio::io::AsyncWrite),
    poll_read(inner, cx, buf) => {
        let mut read_buf = tokio::io::ReadBuf::new(buf);
        match Pin::new(inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    },
    poll_write(inner, cx, buf) => {
        Pin::new(inner).poll_write(cx, buf)
    },
    poll_flush(inner, cx) => {
        Pin::new(inner).poll_flush(cx)
    },
    poll_shutdown(inner, cx) => {
        Pin::new(inner).poll_shutdown(cx)
    },
}

impl_socket_adapter! {
    /// Adapter that wraps a futures-io [`AsyncRead`][futures_io::AsyncRead] +
    /// [`AsyncWrite`][futures_io::AsyncWrite] into a [`Socket`] implementation.
    FuturesStream,
    feature = "_rt-async-io",
    bounds(futures_io::AsyncRead, futures_io::AsyncWrite),
    poll_read(inner, cx, buf) => {
        Pin::new(inner).poll_read(cx, buf)
    },
    poll_write(inner, cx, buf) => {
        Pin::new(inner).poll_write(cx, buf)
    },
    poll_flush(inner, cx) => {
        Pin::new(inner).poll_flush(cx)
    },
    poll_shutdown(inner, cx) => {
        Pin::new(inner).poll_close(cx)
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "_rt-tokio")]
    mod tokio_adapter {
        use super::*;
        use crate::net::Socket;
        use bytes::BytesMut;
        use std::task::Poll;

        #[test]
        fn try_read_returns_would_block_when_empty() {
            let stream = tokio::io::duplex(64).0;
            let mut adapter = TokioStream::new(stream);
            let mut buf = BytesMut::with_capacity(32);
            let err = adapter.try_read(&mut buf).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        }

        #[test]
        fn poll_read_ready_fills_buffer_then_try_read_drains() {
            let (client, mut server) = tokio::io::duplex(64);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                use tokio::io::AsyncWriteExt;
                server.write_all(b"hello world").await.unwrap();

                let mut adapter = TokioStream::new(client);
                let mut buf = BytesMut::with_capacity(32);

                let poll = std::future::poll_fn(|cx| adapter.poll_read_ready(cx)).await;
                assert!(poll.is_ok());

                let n = adapter.try_read(&mut buf).unwrap();
                assert_eq!(&buf[..n], b"hello world");

                let mut buf2 = BytesMut::with_capacity(32);
                let err = adapter.try_read(&mut buf2).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
            });
        }

        #[test]
        fn try_write_writes_data() {
            let (client, mut server) = tokio::io::duplex(64);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                use tokio::io::AsyncReadExt;
                let mut adapter = TokioStream::new(client);

                let n = std::future::poll_fn(|cx| match adapter.try_write(b"test data") {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        match adapter.poll_write_ready(cx) {
                            Poll::Ready(Ok(())) => Poll::Ready(adapter.try_write(b"test data")),
                            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                            Poll::Pending => Poll::Pending,
                        }
                    }
                    other => Poll::Ready(other),
                })
                .await
                .unwrap();

                assert_eq!(n, 9);

                let mut read_buf = vec![0u8; 32];
                let n = server.read(&mut read_buf).await.unwrap();
                assert_eq!(&read_buf[..n], b"test data");
            });
        }

        #[test]
        fn partial_drain_preserves_remaining() {
            let (client, mut server) = tokio::io::duplex(64);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                use tokio::io::AsyncWriteExt;
                server.write_all(b"abcdefghij").await.unwrap();

                let mut adapter = TokioStream::new(client);

                std::future::poll_fn(|cx| adapter.poll_read_ready(cx))
                    .await
                    .unwrap();

                let mut buf = [0u8; 4];
                let n = adapter.try_read(&mut buf.as_mut_slice()).unwrap();
                assert_eq!(n, 4);
                assert_eq!(&buf, b"abcd");

                let mut buf2 = [0u8; 32];
                let n = adapter.try_read(&mut buf2.as_mut_slice()).unwrap();
                assert_eq!(n, 6);
                assert_eq!(&buf2[..6], b"efghij");
            });
        }

        #[test]
        fn poll_read_ready_returns_eof_on_closed_stream() {
            let (client, server) = tokio::io::duplex(64);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                // Drop the server side to close the stream
                drop(server);

                let mut adapter = TokioStream::new(client);
                let err = std::future::poll_fn(|cx| adapter.poll_read_ready(cx))
                    .await
                    .unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
            });
        }

        #[test]
        fn large_data_spans_multiple_buffer_fills() {
            let (client, mut server) = tokio::io::duplex(64 * 1024);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                use tokio::io::AsyncWriteExt;

                // Write more than ADAPTER_BUF_SIZE (8192) bytes
                let data: Vec<u8> = (0..20_000).map(|i| (i % 256) as u8).collect();
                server.write_all(&data).await.unwrap();

                let mut adapter = TokioStream::new(client);
                let mut received = BytesMut::with_capacity(20_000);

                // Read all data through multiple poll_read_ready/try_read cycles
                while received.len() < 20_000 {
                    std::future::poll_fn(|cx| adapter.poll_read_ready(cx))
                        .await
                        .unwrap();
                    // Drain everything available in the internal buffer
                    loop {
                        match adapter.try_read(&mut received) {
                            Ok(_) => {}
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) => panic!("unexpected error: {e}"),
                        }
                    }
                }

                assert_eq!(received.len(), 20_000);
                assert_eq!(&received[..], &data[..]);
            });
        }

    }

    #[cfg(feature = "_rt-async-io")]
    mod futures_adapter {
        use super::*;
        use crate::net::Socket;
        use bytes::BytesMut;
        use std::task::Poll;

        /// A simple in-memory duplex using futures_io traits via Cursor.
        /// We use `futures_util::io::Cursor` which implements AsyncRead + AsyncWrite.
        struct MemStream {
            /// Data available for reading
            read_data: std::io::Cursor<Vec<u8>>,
            /// Written data collected here
            write_data: Vec<u8>,
        }

        impl MemStream {
            fn new(data: &[u8]) -> Self {
                Self {
                    read_data: std::io::Cursor::new(data.to_vec()),
                    write_data: Vec::new(),
                }
            }
        }

        impl futures_io::AsyncRead for MemStream {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                use std::io::Read;
                let n = self.read_data.read(buf)?;
                Poll::Ready(Ok(n))
            }
        }

        impl futures_io::AsyncWrite for MemStream {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                self.write_data.extend_from_slice(buf);
                Poll::Ready(Ok(buf.len()))
            }

            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        #[test]
        fn try_read_returns_would_block_when_empty() {
            let stream = MemStream::new(b"");
            let mut adapter = FuturesStream::new(stream);
            let mut buf = BytesMut::with_capacity(32);
            // Empty stream: poll_read_ready returns UnexpectedEof, try_read returns WouldBlock
            let err = adapter.try_read(&mut buf).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        }

        #[test]
        fn poll_read_ready_fills_buffer_then_try_read_drains() {
            let stream = MemStream::new(b"hello futures");
            let mut adapter = FuturesStream::new(stream);
            let mut buf = BytesMut::with_capacity(32);

            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);

            // poll_read_ready should fill internal buffer
            match adapter.poll_read_ready(&mut cx) {
                Poll::Ready(Ok(())) => {}
                other => panic!("expected Ready(Ok(())), got {:?}", other),
            }

            // try_read should drain from internal buffer
            let n = adapter.try_read(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"hello futures");

            // After draining, try_read should return WouldBlock
            let mut buf2 = BytesMut::with_capacity(32);
            let err = adapter.try_read(&mut buf2).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        }

        #[test]
        fn try_write_writes_data() {
            let stream = MemStream::new(b"");
            let mut adapter = FuturesStream::new(stream);

            let n = adapter.try_write(b"test data").unwrap();
            assert_eq!(n, 9);
        }

        #[test]
        fn partial_drain_preserves_remaining() {
            let stream = MemStream::new(b"abcdefghij");
            let mut adapter = FuturesStream::new(stream);

            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);

            // Fill internal buffer
            match adapter.poll_read_ready(&mut cx) {
                Poll::Ready(Ok(())) => {}
                other => panic!("expected Ready(Ok(())), got {:?}", other),
            }

            // Read only 4 bytes
            let mut buf = [0u8; 4];
            let n = adapter.try_read(&mut buf.as_mut_slice()).unwrap();
            assert_eq!(n, 4);
            assert_eq!(&buf, b"abcd");

            // Remaining 6 bytes should still be available
            let mut buf2 = [0u8; 32];
            let n = adapter.try_read(&mut buf2.as_mut_slice()).unwrap();
            assert_eq!(n, 6);
            assert_eq!(&buf2[..6], b"efghij");
        }

        #[test]
        fn poll_read_ready_returns_eof_on_empty_stream() {
            // MemStream with empty data simulates a closed/EOF stream
            let stream = MemStream::new(b"");
            let mut adapter = FuturesStream::new(stream);

            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);

            match adapter.poll_read_ready(&mut cx) {
                Poll::Ready(Err(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
                other => panic!("expected UnexpectedEof, got {:?}", other),
            }
        }

        #[test]
        fn large_data_spans_multiple_buffer_fills() {
            // Write more than ADAPTER_BUF_SIZE (8192) bytes
            let data: Vec<u8> = (0..20_000).map(|i| (i % 256) as u8).collect();
            let stream = MemStream::new(&data);
            let mut adapter = FuturesStream::new(stream);

            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);

            let mut received = BytesMut::with_capacity(20_000);

            // Read all data through multiple poll_read_ready/try_read cycles
            while received.len() < 20_000 {
                match adapter.poll_read_ready(&mut cx) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(e)) => panic!("unexpected error: {e}"),
                    Poll::Pending => panic!("unexpected Pending"),
                }
                loop {
                    match adapter.try_read(&mut received) {
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => panic!("unexpected error: {e}"),
                    }
                }
            }

            assert_eq!(received.len(), 20_000);
            assert_eq!(&received[..], &data[..]);
        }
    }
}
