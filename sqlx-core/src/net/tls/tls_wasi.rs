//! WASM-native TLS using the wasi-tls component model interface.
//!
//! This module implements TLS for WASM targets via the `wasi:tls/client` WIT
//! interface. The host runtime (e.g. wasmtime) provides the actual TLS
//! implementation; we just wire up the stream plumbing.

use bytes::BytesMut;
use core::task::{Context, Poll};
use futures_util::future::{AbortHandle, Abortable};
use std::io;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use wit_bindgen::rt::async_support;

use crate::io::ReadBuf;
use crate::net::Socket;
use crate::rt::rt_wasip3::WasiPollSender;
use bytes::BufMut as _;

use super::TlsConfig;

// Generate bindings from the wasi-tls WIT specification.
wit_bindgen::generate!({
    path: "src/net/tls/wasi-tls.wit",
    world: "tls-client",
});

/// A TLS-wrapped socket that implements the `Socket` trait.
///
/// Data written by the application flows through the wasi-tls connector's
/// encryption pipeline before being sent over the underlying TCP socket.
/// Incoming TCP data flows through the decryption pipeline before being
/// readable by the application.
pub struct WasiTlsSocket {
    /// Application writes cleartext here; background tasks encrypt and forward to TCP.
    tx: WasiPollSender<Vec<u8>>,
    /// Application reads decrypted data from here.
    rx: mpsc::Receiver<Vec<u8>>,
    /// Buffer for partially consumed received data.
    buf: BytesMut,
    /// Cancellation handle for all background plumbing tasks.
    abort_handle: AbortHandle,
}

impl Drop for WasiTlsSocket {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

impl Socket for WasiTlsSocket {
    fn try_read(&mut self, buf: &mut dyn ReadBuf) -> io::Result<usize> {
        let n = buf.remaining_mut();

        if !self.buf.is_empty() {
            let to_copy = n.min(self.buf.len());
            buf.put_slice(&self.buf.split_to(to_copy));
            return Ok(to_copy);
        }

        match self.rx.try_recv() {
            Ok(rx_vec) => {
                if rx_vec.is_empty() {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                if rx_vec.len() <= n {
                    buf.put_slice(&rx_vec);
                    Ok(rx_vec.len())
                } else {
                    buf.put_slice(&rx_vec[..n]);
                    self.buf.extend_from_slice(&rx_vec[n..]);
                    Ok(n)
                }
            }
            Err(TryRecvError::Empty) => Err(io::ErrorKind::WouldBlock.into()),
            Err(TryRecvError::Disconnected) => Ok(0),
        }
    }

    fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len();
        match self.tx.try_send(buf.to_vec()) {
            Ok(()) => Ok(n),
            Err(_) => Err(io::ErrorKind::WouldBlock.into()),
        }
    }

    fn poll_read_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.buf.is_empty() {
            return Poll::Ready(Ok(()));
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(v)) => {
                if !v.is_empty() {
                    self.buf.extend(v);
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_write_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.tx.poll_reserve(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(())) => Poll::Ready(Err(io::ErrorKind::ConnectionReset.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Perform a TLS handshake over an existing socket using the wasi-tls host
/// implementation.
///
/// The pipeline is:
///
/// ```text
/// App writes cleartext
///   → cleartext_tx (wit stream)
///   → [connector.send] → encrypted_rx (wit stream)
///   → background drain → underlying socket write
///
/// Underlying socket read
///   → background drain → ciphertext_tx (wit stream)
///   → [connector.receive] → decrypted_rx (wit stream)
///   → background drain → App reads decrypted
/// ```
pub async fn handshake<S: Socket + 'static>(
    mut socket: S,
    config: TlsConfig<'_>,
) -> crate::Result<WasiTlsSocket> {
    let hostname = config.hostname.to_string();

    let connector = wasi::tls::client::Connector::new();

    let (mut cleartext_tx, cleartext_rx) = wasip3::wit_stream::new::<u8>();
    let (mut ciphertext_tx, ciphertext_rx) = wasip3::wit_stream::new::<u8>();

    // send_done and recv_done are completion futures — keep them alive for the
    // full duration of the session so the host doesn't tear down the pipeline.
    let (mut encrypted_rx, send_done) = connector.send(cleartext_rx);
    let (mut decrypted_rx, recv_done) = connector.receive(ciphertext_rx);

    let (app_cleartext_tx, mut app_cleartext_rx) = mpsc::channel::<Vec<u8>>(4);
    let (app_decrypted_tx, app_decrypted_rx) = mpsc::channel::<Vec<u8>>(4);

    // Internal channels that bridge the socket IO to the wit-stream pipeline.
    // These decouple the read and write paths so each can be handled in a
    // separate async branch without sharing a `&mut socket`.
    let (tcp_write_tx, mut tcp_write_rx) = mpsc::channel::<Vec<u8>>(4);
    let (tcp_read_tx, mut tcp_read_rx) = mpsc::channel::<Vec<u8>>(4);

    let (abort_handle, abort_registration) = AbortHandle::new_pair();

    async_support::yield_async().await;

    // Socket pump: handles all actual TCP IO in a single async task that owns
    // `socket` exclusively.  Encrypted bytes to send arrive on `tcp_write_rx`;
    // raw bytes received from TCP are forwarded on `tcp_read_tx`.
    let socket_pump = async move {
        use core::future::poll_fn;
        let mut read_storage = [0u8; 4096];
        let mut pending_write: Option<Vec<u8>> = None;
        let mut pending_pos = 0usize;
        loop {
            if pending_write.is_none() {
                match tcp_write_rx.try_recv() {
                    Ok(data) => {
                        pending_write = Some(data);
                        pending_pos = 0;
                    }
                    Err(_) => {
                        tokio::select! {
                            biased;
                            maybe = tcp_write_rx.recv() => {
                                match maybe {
                                    Some(data) => {
                                        pending_write = Some(data);
                                        pending_pos = 0;
                                    }
                                    None => return,
                                }
                            }
                            res = poll_fn(|cx| socket.poll_read_ready(cx)) => {
                                if res.is_err() {
                                    return;
                                }
                                let mut slice: &mut [u8] = &mut read_storage;
                                match socket.try_read(&mut slice) {
                                    Ok(0) => return,
                                    Ok(n) => {
                                        if tcp_read_tx.send(read_storage[..n].to_vec()).await.is_err() {
                                            return;
                                        }
                                    }
                                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                                    Err(_) => return,
                                }
                                continue;
                            }
                        }
                    }
                }
            }

            let data = pending_write.as_ref().unwrap();
            while pending_pos < data.len() {
                if poll_fn(|cx| socket.poll_write_ready(cx)).await.is_err() {
                    return;
                }
                match socket.try_write(&data[pending_pos..]) {
                    Ok(n) => pending_pos += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        async_support::yield_async().await;
                    }
                    Err(_) => return,
                }
            }
            pending_write = None;
        }
    };

    let background = Abortable::new(
        async move {
            futures_util::join!(
                async move { socket_pump.await },
                // Task 1: App cleartext → wit stream (for encryption)
                async {
                    while let Some(data) = app_cleartext_rx.recv().await {
                        let _ = cleartext_tx.write(data).await;
                    }
                    drop(cleartext_tx);
                },
                // Task 2: Encrypted wit stream → tcp_write channel (batch reads)
                async {
                    use wit_bindgen::rt::async_support::StreamResult;
                    loop {
                        let buf = Vec::with_capacity(4096);
                        let (result, buf) = encrypted_rx.read(buf).await;
                        if !buf.is_empty() && tcp_write_tx.send(buf).await.is_err() {
                            break;
                        }
                        match result {
                            StreamResult::Dropped => break,
                            StreamResult::Cancelled | StreamResult::Complete(_) => {}
                        }
                    }
                    drop(encrypted_rx);
                },
                // Task 3: tcp_read channel → ciphertext wit stream (batch writes)
                async {
                    while let Some(data) = tcp_read_rx.recv().await {
                        ciphertext_tx.write_all(data).await;
                    }
                    drop(ciphertext_tx);
                },
                // Task 4: Decrypted wit stream → app-facing channel (batch reads)
                async {
                    use wit_bindgen::rt::async_support::StreamResult;
                    loop {
                        let buf = Vec::with_capacity(4096);
                        let (result, buf) = decrypted_rx.read(buf).await;
                        if !buf.is_empty() && app_decrypted_tx.send(buf).await.is_err() {
                            break;
                        }
                        match result {
                            StreamResult::Dropped => break,
                            StreamResult::Cancelled | StreamResult::Complete(_) => {}
                        }
                    }
                    drop(decrypted_rx);
                    drop(app_decrypted_tx);
                },
                // Hold send_done and recv_done alive until all tasks complete.
                async { let _ = send_done.await; },
                async { let _ = recv_done.await; },
            );
        },
        abort_registration,
    );

    async_support::spawn(async move {
        let _ = background.await;
    });

    // Yield so the spawned background task gets a chance to run before we block
    // on the handshake. Without this, `Connector::connect` may await handshake
    // bytes that never get pumped because no one is polling `background`.
    async_support::yield_async().await;

    let connect_res = wasi::tls::client::Connector::connect(connector, hostname.clone()).await;
    connect_res.map_err(|e| crate::Error::tls(e.to_debug_string()))?;

    Ok(WasiTlsSocket {
        tx: WasiPollSender::new(app_cleartext_tx),
        rx: app_decrypted_rx,
        buf: BytesMut::new(),
        abort_handle,
    })
}
