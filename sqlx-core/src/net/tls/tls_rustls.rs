use std::future;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use rustls::{
    client::{
        danger::{ServerCertVerified, ServerCertVerifier},
        WebPkiServerVerifier,
    },
    crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider},
    pki_types::{
        pem::{self, PemObject},
        CertificateDer, PrivateKeyDer, ServerName, UnixTime,
    },
    CertificateError, ClientConfig, ClientConnection, Error as TlsError, RootCertStore,
};

use crate::error::Error;
use crate::io::ReadBuf;
use crate::net::tls::util::StdSocket;
use crate::net::tls::TlsConfig;
use crate::net::Socket;

pub struct RustlsSocket<S: Socket> {
    inner: StdSocket<S>,
    state: ClientConnection,
    close_notify_sent: bool,
}

impl<S: Socket> RustlsSocket<S> {
    fn poll_complete_io(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match self.state.complete_io(&mut self.inner) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.inner.poll_ready(cx))?;
                }
                ready => return Poll::Ready(ready.map(|_| ())),
            }
        }
    }

    async fn complete_io(&mut self) -> io::Result<()> {
        future::poll_fn(|cx| self.poll_complete_io(cx)).await
    }
}

impl<S: Socket> Socket for RustlsSocket<S> {
    fn try_read(&mut self, buf: &mut dyn ReadBuf) -> io::Result<usize> {
        self.state.reader().read(buf.init_mut())
    }

    fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.state.writer().write(buf) {
            // Returns a zero-length write when the buffer is full.
            Ok(0) => Err(io::ErrorKind::WouldBlock.into()),
            other => other,
        }
    }

    fn poll_read_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_complete_io(cx)
    }

    fn poll_write_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_complete_io(cx)
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Only write out the pending TLS records: once nothing is left to write,
        // `poll_complete_io()` reads and waits for the peer to send something,
        // so a close (which flushes first) would hang if the peer has nothing more to say.
        while self.state.wants_write() {
            match self.state.write_tls(&mut self.inner) {
                // The transport accepts no more; `complete_io()` treats this as EOF too.
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.inner.poll_ready(cx))?;
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
        Poll::Ready(self.inner.flush())
    }

    fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.close_notify_sent {
            self.state.send_close_notify();
            self.close_notify_sent = true;
        }

        ready!(self.poll_complete_io(cx))?;

        // Server can close socket as soon as it receives the connection shutdown request.
        // We shouldn't expect it to stick around for the TLS session to close cleanly.
        // https://security.stackexchange.com/a/82034
        let _ = ready!(self.inner.socket.poll_shutdown(cx));

        Poll::Ready(Ok(()))
    }
}

#[derive(Debug, Clone)]
pub struct RustlsConnector {
    config: Arc<ClientConfig>,
}

pub async fn connector(tls_config: TlsConfig<'_>) -> Result<RustlsConnector, Error> {
    #[cfg(all(
        feature = "_tls-rustls-aws-lc-rs",
        not(feature = "_tls-rustls-ring-webpki"),
        not(feature = "_tls-rustls-ring-native-roots")
    ))]
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    #[cfg(any(
        feature = "_tls-rustls-ring-webpki",
        feature = "_tls-rustls-ring-native-roots"
    ))]
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    // Unwrapping is safe here because we use a default provider.
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap();

    // authentication using user's key and its associated certificate
    let user_auth = match (tls_config.client_cert_path, tls_config.client_key_path) {
        (Some(cert_path), Some(key_path)) => {
            let cert_chain = certs_from_pem(cert_path.data().await?)?;
            let key_der = private_key_from_pem(key_path.data().await?)?;
            Some((cert_chain, key_der))
        }
        (None, None) => None,
        (_, _) => {
            return Err(Error::Configuration(
                "user auth key and certs must be given together".into(),
            ))
        }
    };

    let config = if tls_config.accept_invalid_certs {
        if let Some(user_auth) = user_auth {
            config
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(DummyTlsVerifier { provider }))
                .with_client_auth_cert(user_auth.0, user_auth.1)
                .map_err(Error::tls)?
        } else {
            config
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(DummyTlsVerifier { provider }))
                .with_no_client_auth()
        }
    } else {
        let mut cert_store = import_root_certs();

        if let Some(ca) = tls_config.root_cert_path {
            let data = ca.data().await?;

            for result in CertificateDer::pem_slice_iter(&data) {
                let Ok(cert) = result else {
                    return Err(Error::Tls(format!("Invalid certificate {ca}").into()));
                };

                cert_store.add(cert).map_err(|err| Error::Tls(err.into()))?;
            }
        }

        if tls_config.accept_invalid_hostnames {
            let verifier = WebPkiServerVerifier::builder(Arc::new(cert_store))
                .build()
                .map_err(|err| Error::Tls(err.into()))?;

            if let Some(user_auth) = user_auth {
                config
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoHostnameTlsVerifier { verifier }))
                    .with_client_auth_cert(user_auth.0, user_auth.1)
                    .map_err(Error::tls)?
            } else {
                config
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoHostnameTlsVerifier { verifier }))
                    .with_no_client_auth()
            }
        } else if let Some(user_auth) = user_auth {
            config
                .with_root_certificates(cert_store)
                .with_client_auth_cert(user_auth.0, user_auth.1)
                .map_err(Error::tls)?
        } else {
            config
                .with_root_certificates(cert_store)
                .with_no_client_auth()
        }
    };

    Ok(RustlsConnector {
        config: Arc::new(config),
    })
}

pub async fn handshake<S>(
    socket: S,
    hostname: &str,
    connector: &RustlsConnector,
) -> Result<RustlsSocket<S>, Error>
where
    S: Socket,
{
    let host = ServerName::try_from(hostname.to_owned()).map_err(Error::tls)?;

    let mut socket = RustlsSocket {
        inner: StdSocket::new(socket),
        state: ClientConnection::new(connector.config.clone(), host).map_err(Error::tls)?,
        close_notify_sent: false,
    };

    // Performs the TLS handshake or bails
    socket.complete_io().await?;

    Ok(socket)
}

fn certs_from_pem(pem: Vec<u8>) -> Result<Vec<CertificateDer<'static>>, Error> {
    CertificateDer::pem_slice_iter(&pem)
        .map(|result| result.map_err(|err| Error::Tls(err.into())))
        .collect()
}

fn private_key_from_pem(pem: Vec<u8>) -> Result<PrivateKeyDer<'static>, Error> {
    match PrivateKeyDer::from_pem_slice(&pem) {
        Ok(key) => Ok(key),
        Err(pem::Error::NoItemsFound) => Err(Error::Configuration("no keys found pem file".into())),
        Err(e) => Err(Error::Configuration(e.to_string().into())),
    }
}

#[cfg(all(feature = "webpki-roots", not(feature = "rustls-native-certs")))]
fn import_root_certs() -> RootCertStore {
    RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
}

#[cfg(feature = "rustls-native-certs")]
fn import_root_certs() -> RootCertStore {
    let mut root_cert_store = RootCertStore::empty();

    let load_results = rustls_native_certs::load_native_certs();
    for e in load_results.errors {
        log::warn!("Error loading native certificates: {e:?}");
    }
    for cert in load_results.certs {
        if let Err(e) = root_cert_store.add(cert) {
            log::warn!("rustls failed to parse native certificate: {e:?}");
        }
    }

    root_cert_store
}

// Not currently used but allows for a "tls-rustls-no-roots" feature.
#[cfg(not(any(feature = "rustls-native-certs", feature = "webpki-roots")))]
fn import_root_certs() -> RootCertStore {
    RootCertStore::empty()
}

#[derive(Debug)]
struct DummyTlsVerifier {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for DummyTlsVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
pub struct NoHostnameTlsVerifier {
    verifier: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for NoHostnameTlsVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self.verifier.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(TlsError::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            res => res,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        self.verifier.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        self.verifier.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.verifier.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::task::Waker;

    use rustls::{ConnectionCommon, ServerConfig, ServerConnection};

    use super::*;
    use crate::net::BufferedSocket;

    // A self-signed certificate and its key, for the test server only.
    const CERT: &str = "\
-----BEGIN CERTIFICATE-----
MIIBIjCB1aADAgECAhQRcu62KABJbi99FYkUEAUGt9j9JDAFBgMrZXAwFDESMBAG
A1UEAwwJbG9jYWxob3N0MCAXDTI2MTAwODIyMzE0OVoYDzIxMjYwOTE0MjIzMTQ5
WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwKjAFBgMrZXADIQCIT8wE5M9/LF5HKWU7
hFe/SqknU66oA0FPmjfdziJ/IqM3MDUwFAYDVR0RBA0wC4IJbG9jYWxob3N0MB0G
A1UdDgQWBBRarcSa6Ue2Ob0JlPlE/409LAm6STAFBgMrZXADQQBpNitWpyHm05qH
3Z4w/YG/Ufe05/aDwRAwOKM9snf+pqJ8eG+WnxupOK9oJY/GpmTpW2XSBpVHyGRJ
SmndpuEE
-----END CERTIFICATE-----";

    const KEY: &str = "\
-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEID8jex+k7LAgC5JfhRHaleP+dhL1s4NemFDa1CMKvcuu
-----END PRIVATE KEY-----";

    /// A peer that takes every write and never sends anything.
    #[derive(Default)]
    struct SilentPeer {
        received: Vec<u8>,
    }

    impl Socket for SilentPeer {
        fn try_read(&mut self, _buf: &mut dyn ReadBuf) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }

        fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.received.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn poll_read_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }

        fn poll_write_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Has `to` read and process `records`.
    fn deliver<D>(mut records: &[u8], to: &mut ConnectionCommon<D>) {
        while !records.is_empty() {
            to.read_tls(&mut records).unwrap();
            to.process_new_packets().unwrap();
        }
    }

    /// Delivers the TLS records queued in `from` to `to`.
    fn transfer<A, B>(from: &mut ConnectionCommon<A>, to: &mut ConnectionCommon<B>) {
        let mut records = Vec::new();
        while from.wants_write() {
            from.write_tls(&mut records).unwrap();
        }
        deliver(&records, to);
    }

    #[track_caller]
    fn assert_ready_ok(poll: Poll<io::Result<()>>) {
        assert!(
            matches!(poll, Poll::Ready(Ok(()))),
            "expected `Ready(Ok(()))`, got {poll:?}"
        );
    }

    // A flush with nothing left to write used to read and wait for the peer to send something,
    // so a close, which flushes first, never ended if the peer had nothing more to say (#4449).
    #[test]
    fn flush_and_shutdown_do_not_wait_for_a_silent_peer() {
        let mut cx = Context::from_waker(Waker::noop());

        // The client's config as `sslmode=require` builds it; loading no files, it's ready at once.
        let tls_config = TlsConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: true,
            root_cert_path: None,
            client_cert_path: None,
            client_key_path: None,
        };
        let Poll::Ready(Ok(RustlsConnector { config })) = pin!(connector(tls_config)).poll(&mut cx)
        else {
            panic!("`connector()` should be ready at once");
        };

        let server_config = ServerConfig::builder_with_provider(config.crypto_provider().clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(CERT.as_bytes()).unwrap()],
                PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap(),
            )
            .unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();
        let mut client =
            ClientConnection::new(config, ServerName::try_from("localhost").unwrap()).unwrap();

        // Complete the handshake in memory.
        while client.is_handshaking() || server.is_handshaking() {
            transfer(&mut client, &mut server);
            transfer(&mut server, &mut client);
        }

        let mut socket = RustlsSocket {
            inner: StdSocket::new(SilentPeer::default()),
            state: client,
            close_notify_sent: false,
        };

        // A write is flushed out.
        assert_eq!(socket.try_write(b"ping").unwrap(), 4);
        assert_ready_ok(socket.poll_flush(&mut cx));

        // With nothing left to write, a flush must not wait for the peer.
        assert_ready_ok(socket.poll_flush(&mut cx));

        // `close_hard()` shuts down through `BufferedSocket::shutdown()`, which flushes first.
        let mut socket = BufferedSocket::new(socket);
        assert_ready_ok(pin!(socket.shutdown()).poll(&mut cx));

        // The peer got the write, then the close_notify.
        deliver(&socket.into_inner().inner.socket.received, &mut server);
        let mut received = Vec::new();
        server.reader().read_to_end(&mut received).unwrap();
        assert_eq!(received, b"ping");
        assert!(server.process_new_packets().unwrap().peer_has_closed());
    }
}
