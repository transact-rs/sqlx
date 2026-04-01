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
pub async fn handshake<S: Socket>(
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

    let (abort_handle, abort_registration) = AbortHandle::new_pair();

    async_support::yield_async().await;

    let background = Abortable::new(
        async move {
            futures_util::join!(
                // Task 1: App cleartext → wit stream (for encryption)
                async {
                    while let Some(data) = app_cleartext_rx.recv().await {
                        debug!("wasi-tls: writing {} cleartext bytes to encrypt", data.len());
                        let _ = cleartext_tx.write(data).await;
                    }
                    drop(cleartext_tx);
                },
                // Task 2: Encrypted wit stream → underlying TCP socket
                async {
                    while let Some(byte) = encrypted_rx.next().await {
                        debug!("wasi-tls: forwarding encrypted byte to TCP");
                        // Write encrypted data to the underlying socket.
                        // We poll_write_ready then try_write in a loop.
                        let data = vec![byte];
                        loop {
                            match socket.try_write(&data) {
                                Ok(_) => break,
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    // Yield and retry
                                    async_support::yield_async().await;
                                }
                                Err(e) => {
                                    debug!("wasi-tls: TCP write error: {:?}", e);
                                    return;
                                }
                            }
                        }
                    }
                    drop(encrypted_rx);
                },
                // Task 3: Underlying TCP socket → ciphertext wit stream (for decryption)
                async {
                    let mut read_buf = vec![0u8; 4096];
                    loop {
                        match socket.try_read(&mut read_buf as &mut dyn ReadBuf) {
                            Ok(0) => {
                                // EOF
                                break;
                            }
                            Ok(n) => {
                                debug!("wasi-tls: read {} bytes from TCP, forwarding to decrypt", n);
                                for &b in &read_buf[..n] {
                                    let _ = ciphertext_tx.write(vec![b]).await;
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                async_support::yield_async().await;
                            }
                            Err(e) => {
                                debug!("wasi-tls: TCP read error: {:?}", e);
                                break;
                            }
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
    wasi::tls::client::Connector::connect(connector, &hostname)
        .await
        .map_err(|e| crate::Error::tls(e.message()))?;

    debug!("wasi-tls: handshake complete for {}", hostname);

    Ok(WasiTlsSocket {
        tx: WasiPollSender::new(app_cleartext_tx),
        rx: app_decrypted_rx,
        buf: BytesMut::new(),
        abort_handle,
    })
}
