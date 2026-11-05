#![cfg(any(
    feature = "tls-rustls-aws-lc-rs",
    feature = "tls-rustls-aws-lc-rs-native-roots",
    feature = "tls-rustls-ring-webpki",
    feature = "tls-rustls-ring-native-roots"
))]

use std::env;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore,
    SignatureScheme,
};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgSslMode};
use sqlx::{ConnectOptions, Connection};
use sqlx_core::rustls;

const TEST_CA: &str = "tests/certs/ca.crt";

/// ISRG Root X1, a public root CA that did not sign the test server certificate.
const UNRELATED_CA: &str = "\
-----BEGIN CERTIFICATE-----
MIIFazCCA1OgAwIBAgIRAIIQz7DSQONZRGPgu2OCiwAwDQYJKoZIhvcNAQELBQAw
TzELMAkGA1UEBhMCVVMxKTAnBgNVBAoTIEludGVybmV0IFNlY3VyaXR5IFJlc2Vh
cmNoIEdyb3VwMRUwEwYDVQQDEwxJU1JHIFJvb3QgWDEwHhcNMTUwNjA0MTEwNDM4
WhcNMzUwNjA0MTEwNDM4WjBPMQswCQYDVQQGEwJVUzEpMCcGA1UEChMgSW50ZXJu
ZXQgU2VjdXJpdHkgUmVzZWFyY2ggR3JvdXAxFTATBgNVBAMTDElTUkcgUm9vdCBY
MTCCAiIwDQYJKoZIhvcNAQEBBQADggIPADCCAgoCggIBAK3oJHP0FDfzm54rVygc
h77ct984kIxuPOZXoHj3dcKi/vVqbvYATyjb3miGbESTtrFj/RQSa78f0uoxmyF+
0TM8ukj13Xnfs7j/EvEhmkvBioZxaUpmZmyPfjxwv60pIgbz5MDmgK7iS4+3mX6U
A5/TR5d8mUgjU+g4rk8Kb4Mu0UlXjIB0ttov0DiNewNwIRt18jA8+o+u3dpjq+sW
T8KOEUt+zwvo/7V3LvSye0rgTBIlDHCNAymg4VMk7BPZ7hm/ELNKjD+Jo2FR3qyH
B5T0Y3HsLuJvW5iB4YlcNHlsdu87kGJ55tukmi8mxdAQ4Q7e2RCOFvu396j3x+UC
B5iPNgiV5+I3lg02dZ77DnKxHZu8A/lJBdiB3QW0KtZB6awBdpUKD9jf1b0SHzUv
KBds0pjBqAlkd25HN7rOrFleaJ1/ctaJxQZBKT5ZPt0m9STJEadao0xAH0ahmbWn
OlFuhjuefXKnEgV4We0+UXgVCwOPjdAvBbI+e0ocS3MFEvzG6uBQE3xDk3SzynTn
jh8BCNAw1FtxNrQHusEwMFxIt4I7mKZ9YIqioymCzLq9gwQbooMDQaHWBfEbwrbw
qHyGO0aoSCqI3Haadr8faqU9GY/rOPNk3sgrDQoo//fb4hVC1CLQJ13hef4Y53CI
rU7m2Ys6xt0nUW7/vGT1M0NPAgMBAAGjQjBAMA4GA1UdDwEB/wQEAwIBBjAPBgNV
HRMBAf8EBTADAQH/MB0GA1UdDgQWBBR5tFnme7bl5AFzgAiIyBpY9umbbjANBgkq
hkiG9w0BAQsFAAOCAgEAVR9YqbyyqFDQDLHYGmkgJykIrGF1XIpu+ILlaS/V9lZL
ubhzEFnTIZd+50xx+7LSYK05qAvqFyFWhfFQDlnrzuBZ6brJFe+GnY+EgPbk6ZGQ
3BebYhtF8GaV0nxvwuo77x/Py9auJ/GpsMiu/X1+mvoiBOv/2X/qkSsisRcOj/KK
NFtY2PwByVS5uCbMiogziUwthDyC3+6WVwW6LLv3xLfHTjuCvjHIInNzktHCgKQ5
ORAzI4JMPJ+GslWYHb4phowim57iaztXOoJwTdwJx4nLCgdNbOhdjsnvzqvHu7Ur
TkXWStAmzOVyyghqpZXjFaH3pO3JLF+l+/+sKAIuvtd7u+Nxe5AW0wdeRlN8NwdC
jNPElpzVmbUq4JUagEiuTDkHzsxHpFKVK7q4+63SM1N95R1NbdWhscdCb+ZAJzVc
oyi3B43njTOQ5yOf+1CceWxG1bQVs5ZufpsMljq4Ui0/1lvh+wjChP4kqKOJ2qxq
4RgqsahDYVvTH9w7jXbyLeiNdd8XM2w9U/t7y0Ff/9yi0GE44Za4rF2LN9d11TPA
mRGunUHBcnWEvgJBQl9nJEiU0Zsnvgc/ubhPgXRR4Xq37Z0j4r7g1SgEEzwxA57d
emyPxgcYxn/eR44/KJ4EBs+lVDR3veyJm+kXQ99b21/+jh5Xos1AnX5iItreGCc=
-----END CERTIFICATE-----
";

#[sqlx_macros::test]
async fn it_requires_tls_with_a_custom_config() -> anyhow::Result<()> {
    let Some(options) = tcp_options()? else {
        return Ok(());
    };
    let options = options
        .ssl_mode(PgSslMode::Disable)
        .ssl_client_config(client_config(test_ca()?)?);

    let mut conn = PgConnection::connect_with(&options).await?;
    let ssl: bool = sqlx::query_scalar("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
        .fetch_one(&mut conn)
        .await?;

    assert!(ssl);

    conn.close().await?;

    Ok(())
}

#[sqlx_macros::test]
async fn it_rejects_a_server_outside_the_custom_roots() -> anyhow::Result<()> {
    let Some(options) = tcp_options()? else {
        return Ok(());
    };
    let options = options
        .ssl_mode(PgSslMode::VerifyCa)
        .ssl_root_cert(TEST_CA)
        .ssl_client_config(client_config(pem_roots(UNRELATED_CA.as_bytes())?)?);

    let err = PgConnection::connect_with(&options)
        .await
        .expect_err("server cert is not signed by the configured root");

    let tls_err = match &err {
        sqlx::Error::Io(io) => io.get_ref().and_then(|e| e.downcast_ref::<TlsError>()),
        _ => None,
    };

    assert!(
        matches!(
            tls_err,
            Some(TlsError::InvalidCertificate(
                CertificateError::UnknownIssuer
            ))
        ),
        "expected an unknown issuer error, got {err:?}"
    );

    Ok(())
}

#[sqlx_macros::test]
async fn it_marks_a_custom_config_in_the_url() -> anyhow::Result<()> {
    let options = PgConnectOptions::new_without_pgpass()
        .host("localhost")
        .ssl_client_config(client_config(test_ca()?)?);

    let url = options.to_url_lossy();

    assert!(url
        .query_pairs()
        .any(|(k, v)| k == "danger-custom-ssl-config" && v == "true"));

    let err = PgConnection::connect(url.as_str())
        .await
        .expect_err("URL without its TLS config must not connect");

    assert!(
        matches!(err, sqlx::Error::Configuration(_)),
        "expected a configuration error, got {err:?}"
    );

    Ok(())
}

fn tcp_options() -> anyhow::Result<Option<PgConnectOptions>> {
    let options: PgConnectOptions = env::var("DATABASE_URL")?.parse()?;

    if options.get_socket().is_some() || options.get_host().starts_with('/') {
        return Ok(None);
    }

    Ok(Some(options))
}

/// The test CA from `tests/certs/gen-certs.sh`, which signs the test server certificate.
fn test_ca() -> anyhow::Result<RootCertStore> {
    pem_roots(&std::fs::read(TEST_CA)?)
}

fn pem_roots(pem: &[u8]) -> anyhow::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();

    for cert in CertificateDer::pem_slice_iter(pem) {
        roots.add(cert?)?;
    }

    Ok(roots)
}

/// Trusts only `roots` and presents the test client certificate.
///
/// The test server certificate is issued for `sqlx.test`, not the host the tests connect to,
/// so the hostname check is skipped, as with `sslmode=verify-ca`.
fn client_config(roots: RootCertStore) -> anyhow::Result<Arc<ClientConfig>> {
    let provider = provider();
    let verifier =
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build()?;
    let client_chain =
        CertificateDer::pem_file_iter("tests/certs/client.crt")?.collect::<Result<Vec<_>, _>>()?;
    let client_key = PrivateKeyDer::from_pem_file("tests/certs/keys/client.key")?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(IgnoreHostname(verifier)))
        .with_client_auth_cert(client_chain, client_key)?;

    Ok(Arc::new(config))
}

fn provider() -> Arc<CryptoProvider> {
    #[cfg(any(
        feature = "tls-rustls-aws-lc-rs",
        feature = "tls-rustls-aws-lc-rs-native-roots"
    ))]
    return Arc::new(rustls::crypto::aws_lc_rs::default_provider());

    #[cfg(not(any(
        feature = "tls-rustls-aws-lc-rs",
        feature = "tls-rustls-aws-lc-rs-native-roots"
    )))]
    return Arc::new(rustls::crypto::ring::default_provider());
}

#[derive(Debug)]
struct IgnoreHostname(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for IgnoreHostname {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
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
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}
