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
use tracing::debug;

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
    debug!("wasi-tls: starting handshake for {}", hostname);

    // Create the wasi-tls connector resource.
    let connector = wasi::tls::client::Connector::new();

    // Create two pairs of wit-streams:
    // 1. cleartext pair: app writes cleartext → connector encrypts
    // 2. ciphertext pair: TCP data → connector decrypts
    let (mut cleartext_tx, cleartext_rx) = wasip3::wit_stream::new::<u8>();
    let (mut ciphertext_tx, ciphertext_rx) = wasip3::wit_stream::new::<u8>();

    // Wire up the TLS transform pipelines.
    let (mut encrypted_rx, _send_done) = connector.send(cleartext_rx);
    let (mut decrypted_rx, _recv_done) = connector.receive(ciphertext_rx);

    // App-facing channels: the WasiTlsSocket will use these.
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
        let mut read_storage = [0u8; 4096];
        loop {
            // Try to drain any pending writes first.
            while let Ok(data) = tcp_write_rx.try_recv() {
                let mut pos = 0;
                while pos < data.len() {
                    match socket.try_write(&data[pos..]) {
                        Ok(n) => pos += n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            async_support::yield_async().await;
                        }
                        Err(e) => {
                            debug!("wasi-tls: TCP write error: {:?}", e);
                            return;
                        }
                    }
                }
            }
            // Try to read incoming data.
            {
                let mut slice: &mut [u8] = &mut read_storage;
                match socket.try_read(&mut slice) {
                    Ok(0) => return,
                    Ok(n) => {
                        let _ = tcp_read_tx.send(read_storage[..n].to_vec()).await;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        async_support::yield_async().await;
                    }
                    Err(e) => {
                        debug!("wasi-tls: TCP read error: {:?}", e);
                        return;
                    }
                }
            }
        }
    };

    let background = Abortable::new(
        async move {
            futures_util::join!(
                socket_pump,
                // Task 1: App cleartext → wit stream (for encryption)
                async {
                    while let Some(data) = app_cleartext_rx.recv().await {
                        debug!("wasi-tls: writing {} cleartext bytes to encrypt", data.len());
                        let _ = cleartext_tx.write(data).await;
                    }
                    drop(cleartext_tx);
                },
                // Task 2: Encrypted wit stream → tcp_write channel
                async {
                    while let Some(byte) = encrypted_rx.next().await {
                        let _ = tcp_write_tx.send(vec![byte]).await;
                    }
                    drop(encrypted_rx);
                },
                // Task 3: tcp_read channel → ciphertext wit stream (for decryption)
                async {
                    while let Some(data) = tcp_read_rx.recv().await {
                        debug!("wasi-tls: forwarding {} bytes to decrypt", data.len());
                        for b in data {
                            let _ = ciphertext_tx.write(vec![b]).await;
                        }
                    }
                    drop(ciphertext_tx);
                },
                // Task 4: Decrypted wit stream → app-facing channel
                async {
                    while let Some(byte) = decrypted_rx.next().await {
                        let _ = app_decrypted_tx.send(vec![byte]).await;
                    }
                    drop(decrypted_rx);
                    drop(app_decrypted_tx);
                },
            );
        },
        abort_registration,
    );

    async_support::spawn(async move {
        let _ = background.await;
    });

    // Perform the TLS handshake. This drives the connector to exchange
    // handshake bytes through the send/receive pipelines we set up above.
    wasi::tls::client::Connector::connect(connector, hostname.clone())
        .await
        .map_err(|e| crate::Error::tls(e.to_debug_string()))?;

    debug!("wasi-tls: handshake complete for {}", hostname);

    Ok(WasiTlsSocket {
        tx: WasiPollSender::new(app_cleartext_tx),
        rx: app_decrypted_rx,
        buf: BytesMut::new(),
        abort_handle,
    })
}
