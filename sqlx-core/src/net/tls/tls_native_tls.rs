use std::io::{self, Read, Write};

use crate::io::ReadBuf;
use crate::net::tls::util::StdSocket;
use crate::net::tls::TlsConfig;
use crate::net::Socket;
use crate::rt;
use crate::Error;

use native_tls::{HandshakeError, Identity};
use std::task::{Context, Poll};

pub struct NativeTlsSocket<S: Socket> {
    stream: native_tls::TlsStream<StdSocket<S>>,
}

impl<S: Socket> Socket for NativeTlsSocket<S> {
    fn try_read(&mut self, buf: &mut dyn ReadBuf) -> io::Result<usize> {
        self.stream.read(buf.init_mut())
    }

    fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn poll_read_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream.get_mut().poll_ready(cx)
    }

    fn poll_write_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream.get_mut().poll_ready(cx)
    }

    fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.stream.shutdown() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.stream.get_mut().poll_ready(cx),
            ready => Poll::Ready(ready),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NativeTlsConnector {
    connector: native_tls::TlsConnector,
}

pub async fn connector(config: TlsConfig<'_>) -> crate::Result<NativeTlsConnector> {
    let mut builder = native_tls::TlsConnector::builder();

    builder
        .danger_accept_invalid_certs(config.accept_invalid_certs)
        .danger_accept_invalid_hostnames(config.accept_invalid_hostnames);

    if let Some(root_cert_path) = config.root_cert_path {
        let data = root_cert_path.data().await?;
        let roots = supplied_roots(&data, root_cert_path)?;
        // A SUPPLIED BUNDLE IS THE WHOLE TRUST STORE, matching the rustls
        // path. `add_root_certificate` alone ADDS to the system roots, so a
        // public chain for the configured hostname would still verify;
        // `disable_built_in_roots` is what makes the bundle exclusive. It is
        // set only when a bundle is supplied — a caller that supplies none
        // keeps the system roots exactly as before.
        builder.disable_built_in_roots(true);
        for root in roots {
            builder.add_root_certificate(root);
        }
    }

    // authentication using user's key-file and its associated certificate
    if let (Some(cert_path), Some(key_path)) = (config.client_cert_path, config.client_key_path) {
        let cert_path = cert_path.data().await?;
        let key_path = key_path.data().await?;
        let identity = Identity::from_pkcs8(&cert_path, &key_path).map_err(Error::tls)?;
        builder.identity(identity);
    }

    // The openssl TlsConnector synchronously loads certificates from files.
    // Loading these files can block for tens of milliseconds.
    let connector = rt::spawn_blocking(move || builder.build())
        .await
        .map_err(Error::tls)?;
    Ok(NativeTlsConnector { connector })
}

pub async fn handshake<S: Socket>(
    socket: S,
    hostname: &str,
    connector: &NativeTlsConnector,
) -> crate::Result<NativeTlsSocket<S>> {
    let mut mid_handshake = match connector
        .connector
        .connect(hostname, StdSocket::new(socket))
    {
        Ok(tls_stream) => return Ok(NativeTlsSocket { stream: tls_stream }),
        Err(HandshakeError::Failure(e)) => return Err(Error::tls(e)),
        Err(HandshakeError::WouldBlock(mid_handshake)) => mid_handshake,
    };

    loop {
        mid_handshake.get_mut().ready().await?;

        match mid_handshake.handshake() {
            Ok(tls_stream) => return Ok(NativeTlsSocket { stream: tls_stream }),
            Err(HandshakeError::Failure(e)) => return Err(Error::tls(e)),
            Err(HandshakeError::WouldBlock(mid_handshake_)) => {
                mid_handshake = mid_handshake_;
            }
        }
    }
}

/// Every certificate in a supplied bundle.
///
/// `native_tls::Certificate::from_pem` parses exactly ONE certificate, so a
/// multi-certificate bundle (a root plus an intermediate, or the system
/// bundle) would silently contribute only its first — and with the built-in
/// roots disabled, everything after it would fail at handshake with nothing
/// naming the file. This reads every `CERTIFICATE` section, and refuses a
/// non-whitespace bundle that yields none, the same rule the rustls arm uses.
fn supplied_roots(
    data: &[u8],
    source: &crate::net::tls::CertificateInput,
) -> Result<Vec<native_tls::Certificate>, Error> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;

    // No rustls on this arm (`rustls` is an optional dependency enabled only by
    // the rustls features), so the PEM is split here. Every CERTIFICATE block
    // becomes one certificate; any block that fails to decode refuses the whole
    // bundle, and so does a non-whitespace bundle that yields none.
    let text = String::from_utf8_lossy(data);
    let mut roots = Vec::new();

    for block in text.split("-----BEGIN CERTIFICATE-----").skip(1) {
        let Some((body, _rest)) = block.split_once("-----END CERTIFICATE-----") else {
            return Err(Error::Tls(
                format!("Invalid certificate {source}: unterminated block").into(),
            ));
        };
        let decoded = STANDARD
            .decode(body.split_whitespace().collect::<String>())
            .map_err(|err| Error::Tls(format!("Invalid certificate {source}: {err}").into()))?;
        roots
            .push(native_tls::Certificate::from_der(&decoded).map_err(|err| {
                Error::Tls(format!("Invalid certificate {source}: {err}").into())
            })?);
    }

    if roots.is_empty() && !data.iter().all(u8::is_ascii_whitespace) {
        return Err(Error::Tls(
            format!("No certificate found in {source}").into(),
        ));
    }

    Ok(roots)
}

#[cfg(all(test, feature = "_tls-native-tls"))]
mod supplied_roots_tests {
    use super::*;
    use crate::net::tls::CertificateInput;

    // Two self-signed certificates, for the trust-store tests only.
    const CA_A: &str = "-----BEGIN CERTIFICATE-----\nMIIDAjCCAeqgAwIBAgIUC2yl3UmpqaiPbRPW6cYDudJqO7cwDQYJKoZIhvcNAQEL\nBQAwGTEXMBUGA1UEAwwOc3FseC10ZXN0LWNhLTEwHhcNMjYxMTI3MDg1MTI0WhcN\nMzYxMTI0MDg1MTI0WjAZMRcwFQYDVQQDDA5zcWx4LXRlc3QtY2EtMTCCASIwDQYJ\nKoZIhvcNAQEBBQADggEPADCCAQoCggEBALU1ME00bOX20WVlUSbMO/EkcmToBhNN\nwZeejxMBCSe+lWyZRd35qtR0wjiAIw9/29eQ+RlbDCz+Ur3kpOJ+f6P4of0zk1Dg\n5561/f9QgVIsJ1mO7Ldy/juq5SWV4/82TE4k+gvMoEHRuMutVET1tsyX+9ia9B9z\nZeRT8b0fRsyEeUQZZqEzbFXhxB15Yc/ScXSxp+aZEibMydUsAM8N/+zzxeYKFUTD\nCSuVOtxCNdDcgSF21RrVTafEOKX8MU9C1WH0mO7K1YVi3ms1bkmIrBagm2ldM9Fb\nxcsK2dYozmWRxRXIu0umH2j06kRH3NUrkZv+ehUozZOC5PPvckkIcT8CAwEAAaNC\nMEAwHQYDVR0OBBYEFKm40mqfkLJLWaBAMzJ1aMoRuC85MA8GA1UdEwEB/wQFMAMB\nAf8wDgYDVR0PAQH/BAQDAgEGMA0GCSqGSIb3DQEBCwUAA4IBAQAhmOwHf6kUZc4L\nznzOSNrV0pDs9vx/oYvR53PgCFT14QPAcAC1bp3ZLUnZmlzXcYK/Wt+pcgyMXhxk\nEM1LzmvvutK6auiSDfQ0jPnvAtSDbpD6winZ6BdnjxmH5DnV0Hody3gILnbxg4Qh\ncxgn3IsjrAm9BLdssoO8WgNM9tvwBqcGkhByQm6OoFi5CAHeBh1XAdK/XdM/AyvL\n4QcB9zWLd57RLltz4Brbmh8twhksE51/gi2CzfzQ10rffEhsfEDyXALJ/KIBtJLA\nH8n3JMrggTzkQc8Y1TepkeevpeZ9cXXBDFWPvvd4os2fCxnc9oLvn9dLIX51I066\n1cOckJhQ\n-----END CERTIFICATE-----";
    const CA_B: &str = "-----BEGIN CERTIFICATE-----\nMIIDAjCCAeqgAwIBAgIUSw5EoL3n8Xx0a8u1YHtrh3JrKNkwDQYJKoZIhvcNAQEL\nBQAwGTEXMBUGA1UEAwwOc3FseC10ZXN0LWNhLTIwHhcNMjYxMTI3MDg1MTI1WhcN\nMzYxMTI0MDg1MTI1WjAZMRcwFQYDVQQDDA5zcWx4LXRlc3QtY2EtMjCCASIwDQYJ\nKoZIhvcNAQEBBQADggEPADCCAQoCggEBALacNkNHSwqSgquVXyiwTgN/Ps5zAiOi\nI9+ze/mSdeUHloBhsRh8AShC14V1yr4PUufXGOEnmihQAiuE3rYiGt3mkXecRMXe\n5rwQNNYA04kaLXXFWSrMErtE9WRVk1vnnVHyhg7ieVtj92+NqYx6uYWmf7RkcA+6\n9S9cXuzjuzHMqG4qUvC2qFPJjLrJETMkgVEXkPyflwDyRkyAuig5K1tnqWsYt1wr\nK9a1pIpi2sBjrvUBBVIYkbOKac6RTUyx91+ELQPjj/HZM+Z3WbkEECbvV4SbuF7C\nmVrvtZNievG2X7/gbWJ6QfqKvfIJ+pTK0nK3r7nfW3cWXik32D0y9isCAwEAAaNC\nMEAwHQYDVR0OBBYEFJ8PSXoDC5QaS6XU3AwLq/vjM6wFMA8GA1UdEwEB/wQFMAMB\nAf8wDgYDVR0PAQH/BAQDAgEGMA0GCSqGSIb3DQEBCwUAA4IBAQClHE5ixzZy9nh7\na8laOznmud6JzJ3ni/HprNnQCguU25SAFmueh93ZW5izBpA2S1ViwaFS9bR4C2fY\ni83iISmbGX1iXn6YDQuTa4BWPXbf9invw0fK2ASu5W8IF6m85j9A4faDBmhSyXmR\nq6bswWDoOtYNUaojDXUOysjQIYzmy5zhiNvrzaHrvBH5s+4s/MzkPonOqTX5fLjn\ngCGliRK2GizeaRUmPgI4oPtCZFWhR4qGm45jl906AQQyFlFJKSOVhLjjeUo6jROu\n4MJG7V1rgTs2mI8gCwKUOBYplXfX2gh4lnF+pzz73Z4AXTKg4aEVdN5v94m09OcA\nBjZVB0yH\n-----END CERTIFICATE-----";

    /// A two-certificate bundle yields BOTH certificates (the single-cert
    /// parse would silently keep only the first), and an empty or
    /// whitespace-only bundle yields none without an error.
    #[test]
    fn a_bundle_yields_every_certificate() {
        let bundle = [CA_A, CA_B].concat();
        let input = CertificateInput::Inline(bundle.clone().into_bytes());
        assert_eq!(supplied_roots(bundle.as_bytes(), &input).unwrap().len(), 2);

        let empty = CertificateInput::Inline(Vec::new());
        assert_eq!(supplied_roots(b"", &empty).unwrap().len(), 0);
    }

    /// Non-PEM input yields no certificate at all, so it is refused by name
    /// rather than becoming an empty (unusable) store.
    #[test]
    fn a_bundle_with_no_certificate_is_refused() {
        let input = CertificateInput::Inline(b"not a certificate\n".to_vec());
        // `native_tls::Certificate` is not Debug, so `unwrap_err()` cannot be
        // used here; take the error through Option instead.
        let error = supplied_roots(b"not a certificate\n", &input)
            .err()
            .expect("a bundle with no certificate must be refused");
        assert!(
            format!("{error}").contains("No certificate found"),
            "got: {error}"
        );
    }
}
