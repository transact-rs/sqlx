use crate::error::Error;
use crate::net::{Socket, SocketExt};
use bytes::BytesMut;
use futures_util::Sink;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::{cmp, io};

use crate::io::{read_from, AsyncRead, ProtocolDecode, ProtocolEncode};

// Tokio, async-std, and std all use this as the default capacity for their buffered I/O.
const DEFAULT_BUF_SIZE: usize = 8192;

pub struct BufferedSocket<S> {
    socket: S,
    write_buf: WriteBuffer,
    read_buf: ReadBuffer,
    wants_bytes: usize,
}

pub struct WriteBuffer {
    buf: Vec<u8>,
    bytes_written: usize,
    bytes_flushed: usize,
}

pub struct ReadBuffer {
    read: BytesMut,
    available: BytesMut,
}

impl<S: Socket> BufferedSocket<S> {
    pub fn new(socket: S) -> Self
    where
        S: Sized,
    {
        BufferedSocket {
            socket,
            write_buf: WriteBuffer {
                buf: Vec::with_capacity(DEFAULT_BUF_SIZE),
                bytes_written: 0,
                bytes_flushed: 0,
            },
            read_buf: ReadBuffer {
                read: BytesMut::new(),
                available: BytesMut::with_capacity(DEFAULT_BUF_SIZE),
            },
            wants_bytes: 0,
        }
    }

    pub async fn read_buffered(&mut self, len: usize) -> Result<BytesMut, Error> {
        self.try_read(|buf| {
            Ok(if buf.len() < len {
                ControlFlow::Continue(len)
            } else {
                ControlFlow::Break(buf.split_to(len))
            })
        })
        .await
    }

    pub fn poll_try_read<F, R>(
        &mut self,
        cx: &mut Context<'_>,
        mut try_read: F,
    ) -> Poll<Result<R, Error>>
    where
        F: FnMut(&mut BytesMut) -> Result<ControlFlow<R, usize>, Error>,
    {
        loop {
            // Ensure we have enough bytes, only read if we want bytes.
            ready!(self.poll_handle_read(cx)?);

            match try_read(&mut self.read_buf.read)? {
                ControlFlow::Continue(read_len) => self.wants_bytes = read_len,
                ControlFlow::Break(ret) => return Poll::Ready(Ok(ret)),
            };
        }
    }

    /// Retryable read operation.
    ///
    /// The callback should check the contents of the buffer passed to it and either:
    ///
    /// * Remove a full message from the buffer and return [`ControlFlow::Break`], or:
    /// * Return [`ControlFlow::Continue`] with the expected _total_ length of the buffer,
    ///   _without_ modifying it.
    ///
    /// Cancel-safe as long as the callback does not modify the passed `BytesMut`
    /// before returning [`ControlFlow::Continue`].
    pub async fn try_read<F, R>(&mut self, mut try_read: F) -> Result<R, Error>
    where
        F: FnMut(&mut BytesMut) -> Result<ControlFlow<R, usize>, Error>,
    {
        loop {
            let read_len = match try_read(&mut self.read_buf.read)? {
                ControlFlow::Continue(read_len) => read_len,
                ControlFlow::Break(ret) => return Ok(ret),
            };

            self.read_buf.read(read_len, &mut self.socket).await?;
        }
    }

    pub fn write_buffer(&self) -> &WriteBuffer {
        &self.write_buf
    }

    pub fn write_buffer_mut(&mut self) -> &mut WriteBuffer {
        &mut self.write_buf
    }

    pub async fn read<'de, T>(&mut self, byte_len: usize) -> Result<T, Error>
    where
        T: ProtocolDecode<'de, ()>,
    {
        self.read_with(byte_len, ()).await
    }

    pub async fn read_with<'de, T, C>(&mut self, byte_len: usize, context: C) -> Result<T, Error>
    where
        T: ProtocolDecode<'de, C>,
    {
        T::decode_with(self.read_buffered(byte_len).await?.freeze(), context)
    }

    #[inline(always)]
    pub fn write<'en, T>(&mut self, value: T) -> Result<(), Error>
    where
        T: ProtocolEncode<'en, ()>,
    {
        self.write_with(value, ())
    }

    #[inline(always)]
    pub fn write_with<'en, T, C>(&mut self, value: T, context: C) -> Result<(), Error>
    where
        T: ProtocolEncode<'en, C>,
    {
        value.encode_with(self.write_buf.buf_mut(), context)?;
        self.write_buf.bytes_written = self.write_buf.buf.len();
        self.write_buf.sanity_check();

        Ok(())
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        while !self.write_buf.is_empty() {
            let written = self.socket.write(self.write_buf.get()).await?;
            // Consume does the sanity check.
            self.write_buf.consume(written);
        }

        self.socket.flush().await?;

        Ok(())
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.flush().await?;
        self.socket.shutdown().await
    }

    pub fn shrink_buffers(&mut self) {
        // Won't drop data still in the buffer.
        self.write_buf.shrink();
        self.read_buf.shrink();
    }

    pub fn into_inner(self) -> S {
        self.socket
    }

    pub fn boxed(self) -> BufferedSocket<Box<dyn Socket>> {
        BufferedSocket {
            socket: Box::new(self.socket),
            write_buf: self.write_buf,
            read_buf: self.read_buf,
            wants_bytes: self.wants_bytes,
        }
    }

    fn poll_handle_read(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Because of how `BytesMut` works, we should only be shifting capacity back and forth
        // between `read` and `available` unless we have to read an oversize message.

        while self.read_buf.len() < self.wants_bytes {
            self.read_buf
                .reserve(self.wants_bytes - self.read_buf.len());

            let read = ready!(self.socket.poll_read(cx, &mut self.read_buf.available)?);

            if read == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "expected to read {} bytes, got {} bytes at EOF",
                        self.wants_bytes,
                        self.read_buf.len()
                    ),
                )));
            }

            self.read_buf.advance(read);
        }

        // we've read at least enough for `wants_bytes`, so we don't want more.
        self.wants_bytes = 0;
        Poll::Ready(Ok(()))
    }
}

impl WriteBuffer {
    fn sanity_check(&self) {
        assert_ne!(self.buf.capacity(), 0);
        assert!(self.bytes_written <= self.buf.len());
        assert!(self.bytes_flushed <= self.bytes_written);
    }

    pub fn buf_mut(&mut self) -> &mut Vec<u8> {
        self.buf.truncate(self.bytes_written);
        self.sanity_check();
        &mut self.buf
    }

    pub fn init_remaining_mut(&mut self) -> &mut [u8] {
        self.buf.resize(self.buf.capacity(), 0);
        self.sanity_check();
        &mut self.buf[self.bytes_written..]
    }

    pub fn put_slice(&mut self, slice: &[u8]) {
        // If we already have an initialized area that can fit the slice,
        // don't change `self.buf.len()`
        if let Some(dest) = self.buf[self.bytes_written..].get_mut(..slice.len()) {
            dest.copy_from_slice(slice);
        } else {
            self.buf.truncate(self.bytes_written);
            self.buf.extend_from_slice(slice);
        }
        self.advance(slice.len());
        self.sanity_check();
    }

    pub fn advance(&mut self, amt: usize) {
        let new_bytes_written = self
            .bytes_written
            .checked_add(amt)
            .expect("self.bytes_written + amt overflowed");

        assert!(new_bytes_written <= self.buf.len());

        self.bytes_written = new_bytes_written;

        self.sanity_check();
    }

    /// Read into the buffer from `source`, returning the number of bytes read.
    ///
    /// The buffer is automatically advanced by the number of bytes read.
    pub async fn read_from(&mut self, source: impl AsyncRead + Unpin) -> io::Result<usize> {
        let read = read_from(source, self.buf_mut()).await?;

        if read > 0 {
            self.advance(read);
        }

        Ok(read)
    }

    pub fn is_empty(&self) -> bool {
        self.bytes_flushed >= self.bytes_written
    }

    pub fn is_full(&self) -> bool {
        self.bytes_written == self.buf.len()
    }

    pub fn get(&self) -> &[u8] {
        &self.buf[self.bytes_flushed..self.bytes_written]
    }

    pub fn get_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.bytes_flushed..self.bytes_written]
    }

    pub fn shrink(&mut self) {
        if self.bytes_flushed > 0 {
            // Move any data that remains to be flushed to the beginning of the buffer,
            // if necessary.
            self.buf
                .copy_within(self.bytes_flushed..self.bytes_written, 0);
            self.bytes_written -= self.bytes_flushed;
            self.bytes_flushed = 0
        }

        // Drop excess capacity.
        self.buf
            .truncate(cmp::max(self.bytes_written, DEFAULT_BUF_SIZE));
        self.buf.shrink_to_fit();
    }

    fn consume(&mut self, amt: usize) {
        let new_bytes_flushed = self
            .bytes_flushed
            .checked_add(amt)
            .expect("self.bytes_flushed + amt overflowed");

        assert!(new_bytes_flushed <= self.bytes_written);

        self.bytes_flushed = new_bytes_flushed;

        if self.bytes_flushed == self.bytes_written {
            // Reset cursors to zero if we've consumed the whole buffer
            self.bytes_flushed = 0;
            self.bytes_written = 0;
        }

        self.sanity_check();
    }
}

impl ReadBuffer {
    async fn read(&mut self, len: usize, socket: &mut impl Socket) -> io::Result<()> {
        // Because of how `BytesMut` works, we should only be shifting capacity back and forth
        // between `read` and `available` unless we have to read an oversize message.
        while self.read.len() < len {
            self.reserve(len - self.read.len());

            let read = socket.read(&mut self.available).await?;

            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "expected to read {} bytes, got {} bytes at EOF",
                        len,
                        self.read.len()
                    ),
                ));
            }

            self.advance(read);
        }

        Ok(())
    }

    fn reserve(&mut self, amt: usize) {
        if let Some(additional) = amt.checked_sub(self.available.capacity()) {
            self.available.reserve(additional);
        }
    }

    fn advance(&mut self, amt: usize) {
        self.read.unsplit(self.available.split_to(amt));
    }

    fn shrink(&mut self) {
        if self.available.capacity() > DEFAULT_BUF_SIZE {
            // `BytesMut` doesn't have a way to shrink its capacity,
            // but we only use `available` for spare capacity anyway so we can just replace it.
            //
            // If `self.read` still contains data on the next call to `advance` then this might
            // force a memcpy as they'll no longer be pointing to the same allocation,
            // but that's kind of unavoidable.
            //
            // The `async-std` impl of `Socket` will also need to re-zero the buffer,
            // but that's also kind of unavoidable.
            //
            // We should be warning the user not to call this often.
            self.available = BytesMut::with_capacity(DEFAULT_BUF_SIZE);
        }
    }

    fn len(&self) -> usize {
        self.read.len()
    }
}

impl<S: Socket> Sink<&[u8]> for BufferedSocket<S> {
    type Error = crate::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        if self.write_buf.bytes_written >= DEFAULT_BUF_SIZE {
            self.poll_flush(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: &[u8]) -> crate::Result<()> {
        self.write_buffer_mut().put_slice(item);
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        let this = &mut *self;

        while !this.write_buf.is_empty() {
            let written = ready!(this.socket.poll_write(cx, this.write_buf.get())?);
            // Consume does the sanity check.
            this.write_buf.consume(written);
        }
        this.socket.poll_flush(cx).map_err(Into::into)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.socket.poll_shutdown(cx).map_err(Into::into)
    }
}
