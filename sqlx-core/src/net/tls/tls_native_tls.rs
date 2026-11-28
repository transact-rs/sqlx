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
        // ANDROID CANNOT HONOUR EXCLUSIVITY, SO A BUNDLE IS REFUSED THERE
        // (verified in native-tls 0.2.18 `imp/openssl.rs:313-329`):
        // `disable_built_in_roots` clears the OpenSSL store and adds the
        // supplied certificates, and the backend then UNCONDITIONALLY reloads
        // `/system/etc/security/cacerts`. A supplied bundle would therefore
        // still trust every public root on that platform. Refusing is
        // fail-closed and loud; the alternative — a bundle that is silently
        // additive — is the defect this change exists to close. `cfg!` rather
        // than `#[cfg]` so the arm is TYPE-CHECKED on every target.
        if cfg!(target_os = "android") {
            let _ = root_cert_path;
            return Err(Error::Configuration(
                "a supplied root certificate cannot be made exclusive on \
                 Android: the native-tls backend reloads the system roots after \
                 honouring disable_built_in_roots. Use the rustls backend, or \
                 supply no root certificate and rely on the system roots."
                    .into(),
            ));
        }

        let data = root_cert_path.data().await?;
        let roots = supplied_roots(&data, root_cert_path)?;
        // A SUPPLIED BUNDLE IS THE WHOLE TRUST STORE, matching the rustls
        // path. `add_root_certificate` alone ADDS to the system roots, so a
        // public chain for the configured hostname would still verify;
        // `disable_built_in_roots` is what makes the bundle exclusive. It is
        // set only when a bundle is supplied — a caller that supplies none
        // keeps the system roots exactly as before.
        // NOT UNIT-TESTABLE, AND SAID SO: `native_tls::TlsConnectorBuilder`
        // exposes no getter, so no test can observe this line — deleting it
        // would leave every test in this file green while restoring the
        // additive system roots. It is held by review and by the Android
        // refusal above, which exists because this flag is defeated there.
        builder.disable_built_in_roots(true);
        for root in roots {
            let certificate = native_tls::Certificate::from_der(&root).map_err(|err| {
                Error::Tls(format!("Invalid certificate {root_cert_path}: {err}").into())
            })?;
            builder.add_root_certificate(certificate);
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
) -> Result<Vec<Vec<u8>>, Error> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;

    // No rustls on this arm (`rustls` is an optional dependency enabled only by
    // the rustls features), so the PEM is split here. The rules, each measured
    // against what the rustls parser does:
    //
    //   * EVERY `-----BEGIN …-----` line must be a WELL-FORMED section start.
    //     A near-miss (`-----BEGIN CERTIFICATE----`, a stray byte in the
    //     header) would otherwise leave its block undetected while the blocks
    //     before it are kept — a bundle that is silently PARTIAL, which is the
    //     one failure mode this function exists to refuse.
    //   * Every section's body must DECODE, whatever its kind — a malformed
    //     PRIVATE KEY section is a broken bundle, the same way rustls reads it.
    //   * Every CERTIFICATE section becomes one root, via `from_der`.
    //   * An empty or all-whitespace bundle yields no roots; a non-whitespace
    //     bundle that yields none is refused by name.
    let text = String::from_utf8_lossy(data);
    let mut roots = Vec::new();
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("-----BEGIN") else {
            continue;
        };
        // A line that OPENS like a section must BE one. `-----BEGIN CERTIFICATE----`
        // (four dashes), a stray byte in the kind, or a missing space is a
        // damaged bundle, not prose — and treating it as prose would keep the
        // blocks before it and silently drop this one.
        let kind = rest
            .strip_prefix(' ')
            .and_then(|r| r.strip_suffix("-----"))
            .filter(|kind| {
                !kind.is_empty()
                    && kind
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b == b' ' || b.is_ascii_digit())
            });
        let Some(kind) = kind else {
            return Err(Error::Tls(
                format!("Invalid certificate {source}: malformed PEM section header").into(),
            ));
        };
        let terminator = format!("-----END {kind}-----");
        let mut body = String::new();
        let mut terminated = false;
        for line in lines.by_ref() {
            if line.trim() == terminator {
                terminated = true;
                break;
            }
            body.push_str(line.trim());
        }
        if !terminated {
            return Err(Error::Tls(
                format!("Invalid certificate {source}: unterminated {kind} block").into(),
            ));
        }

        let decoded = STANDARD
            .decode(body.as_bytes())
            .map_err(|err| Error::Tls(format!("Invalid certificate {source}: {err}").into()))?;
        if kind == "CERTIFICATE" {
            // VALIDATED HERE, CONSTRUCTED AT THE CALL SITE: `from_der` is what
            // proves the bytes are a certificate, and returning the DER lets the
            // tests prove the roots are DISTINCT — `native_tls::Certificate`
            // exposes no identity, so a parser that pushed the same certificate
            // twice would satisfy a length-only assertion.
            native_tls::Certificate::from_der(&decoded)
                .map_err(|err| Error::Tls(format!("Invalid certificate {source}: {err}").into()))?;
            roots.push(decoded);
        }
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
        let bundle = format!("{CA_A}\n{CA_B}\n");
        let input = CertificateInput::Inline(bundle.clone().into_bytes());
        let roots = supplied_roots(bundle.as_bytes(), &input).unwrap();
        assert_eq!(roots.len(), 2);
        assert_ne!(
            roots[0], roots[1],
            "the two roots must be DISTINCT: a parser that pushed the same certificate twice \
             would satisfy a length-only assertion"
        );

        let empty = CertificateInput::Inline(Vec::new());
        assert_eq!(supplied_roots(b"", &empty).unwrap().len(), 0);
    }

    /// A LATER SECTION WITH A DAMAGED HEADER must refuse the whole bundle.
    /// `-----BEGIN CERTIFICATE----` (four trailing dashes) is not the marker, so
    /// a splitter that only looks for the exact marker would keep CA_A and drop
    /// the damaged block — a silently PARTIAL bundle.
    #[test]
    fn a_bundle_with_a_damaged_header_is_refused() {
        let damaged =
            format!("{CA_A}\n-----BEGIN CERTIFICATE----\nAAAA\n-----END CERTIFICATE-----\n");
        let input = CertificateInput::Inline(damaged.clone().into_bytes());
        let error = supplied_roots(damaged.as_bytes(), &input)
            .err()
            .expect("a damaged section header must refuse the bundle");
        assert!(
            format!("{error}").contains("malformed PEM section header"),
            "got: {error}"
        );
    }

    /// A section that is not a certificate still has to DECODE: a broken
    /// PRIVATE KEY block is a broken bundle, the way the rustls parser reads it.
    #[test]
    fn a_malformed_non_certificate_section_is_refused() {
        let broken = format!(
            "{CA_A}\n-----BEGIN PRIVATE KEY-----\nnot base64!\n-----END PRIVATE KEY-----\n"
        );
        let input = CertificateInput::Inline(broken.clone().into_bytes());
        let error = supplied_roots(broken.as_bytes(), &input)
            .err()
            .expect("a malformed section body must refuse the bundle");
        assert!(
            format!("{error}").contains("Invalid certificate"),
            "got: {error}"
        );
    }

    /// A whitespace-only bundle is the EMPTY case: no roots, no error.
    #[test]
    fn a_whitespace_only_bundle_has_no_roots() {
        let input = CertificateInput::Inline(b"  \n\t\n".to_vec());
        assert_eq!(supplied_roots(b"  \n\t\n", &input).unwrap().len(), 0);
    }

    /// Non-PEM input yields no certificate at all, so it is refused by name
    /// rather than becoming an empty (unusable) store.
    #[test]
    fn a_bundle_with_no_certificate_is_refused() {
        let input = CertificateInput::Inline(b"not a certificate\n".to_vec());
        let error = supplied_roots(b"not a certificate\n", &input)
            .err()
            .expect("a bundle with no certificate must be refused");
        assert!(
            format!("{error}").contains("No certificate found"),
            "got: {error}"
        );
    }
}
