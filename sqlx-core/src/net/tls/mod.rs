#![allow(dead_code)]

use std::path::PathBuf;

use crate::error::Error;
use crate::net::socket::WithSocket;
use crate::net::Socket;

#[cfg(feature = "_tls-rustls")]
mod tls_rustls;

#[cfg(feature = "_tls-native-tls")]
mod tls_native_tls;

mod util;

/// X.509 Certificate input, either a file path or a PEM encoded inline certificate(s).
#[derive(Clone, Debug)]
pub enum CertificateInput {
    /// PEM encoded certificate(s)
    Inline(Vec<u8>),
    /// Path to a file containing PEM encoded certificate(s)
    File(PathBuf),
}

impl From<String> for CertificateInput {
    fn from(value: String) -> Self {
        // Leading and trailing whitespace/newlines
        let trimmed = value.trim();

        // Heuristic for PEM encoded inputs:
        // https://tools.ietf.org/html/rfc7468
        if trimmed.starts_with("-----BEGIN") && trimmed.ends_with("-----") {
            CertificateInput::Inline(value.as_bytes().to_vec())
        } else {
            CertificateInput::File(PathBuf::from(value))
        }
    }
}

impl CertificateInput {
    async fn data(&self) -> Result<Vec<u8>, std::io::Error> {
        use crate::fs;
        match self {
            CertificateInput::Inline(v) => Ok(v.clone()),
            CertificateInput::File(path) => fs::read(path).await,
        }
    }
}

impl std::fmt::Display for CertificateInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertificateInput::Inline(v) => write!(f, "{}", String::from_utf8_lossy(v.as_slice())),
            CertificateInput::File(path) => write!(f, "file: {}", path.display()),
        }
    }
}

/// A TLS client configuration built by the application.
///
/// With the Rustls backend this wraps a [`rustls::ClientConfig`], so the application controls
/// the crypto provider, the trust anchors, server certificate verification, and client
/// authentication, including a client certificate resolver whose key never leaves a hardware
/// token or HSM (`rustls::client::ResolvesClientCert`).
///
/// SQLx builds no part of the configuration: the SSL mode, root certificate, and client
/// certificate and key options do not apply to it. It is made from a `rustls::ClientConfig` or an
/// `Arc` of one through [`IntoPrebuiltTlsConfig`].
///
/// It can only be constructed when SQLx is built with one of the `tls-rustls*` features and
/// without `tls-native-tls`. With both, native-tls is the backend, [`IntoPrebuiltTlsConfig`] has
/// no implementations, and code that passes a Rustls configuration does not compile.
#[derive(Clone, Debug)]
pub struct PrebuiltTlsConfig(PrebuiltTlsConfigInner);

#[derive(Clone, Debug)]
enum PrebuiltTlsConfigInner {
    #[cfg(all(feature = "_tls-rustls", not(feature = "_tls-native-tls")))]
    Rustls(std::sync::Arc<rustls::ClientConfig>),
}

/// Converts a Rustls client configuration into a [`PrebuiltTlsConfig`].
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be used as a prebuilt TLS configuration in this build of SQLx",
    label = "requires the Rustls TLS backend",
    note = "a prebuilt `rustls::ClientConfig` requires a `tls-rustls*` feature without \
            `tls-native-tls`; when both are enabled, native-tls is the TLS backend"
)]
pub trait IntoPrebuiltTlsConfig {
    /// Wraps `self` for use.
    fn into_prebuilt_tls_config(self) -> PrebuiltTlsConfig;
}

#[cfg(all(feature = "_tls-rustls", not(feature = "_tls-native-tls")))]
impl IntoPrebuiltTlsConfig for std::sync::Arc<rustls::ClientConfig> {
    fn into_prebuilt_tls_config(self) -> PrebuiltTlsConfig {
        PrebuiltTlsConfig(PrebuiltTlsConfigInner::Rustls(self))
    }
}

#[cfg(all(feature = "_tls-rustls", not(feature = "_tls-native-tls")))]
impl IntoPrebuiltTlsConfig for rustls::ClientConfig {
    fn into_prebuilt_tls_config(self) -> PrebuiltTlsConfig {
        PrebuiltTlsConfig(PrebuiltTlsConfigInner::Rustls(std::sync::Arc::new(self)))
    }
}

pub struct TlsConfig<'a> {
    pub accept_invalid_certs: bool,
    pub accept_invalid_hostnames: bool,
    pub root_cert_path: Option<&'a CertificateInput>,
    pub client_cert_path: Option<&'a CertificateInput>,
    pub client_key_path: Option<&'a CertificateInput>,
}

#[cfg(feature = "_tls-native-tls")]
pub use self::tls_native_tls::NativeTlsConnector as TlsConnector;
#[cfg(all(feature = "_tls-rustls", not(feature = "_tls-native-tls")))]
pub use self::tls_rustls::RustlsConnector as TlsConnector;
#[cfg(not(any(feature = "_tls-native-tls", feature = "_tls-rustls")))]
#[derive(Debug, Clone)]
pub struct TlsConnector(std::convert::Infallible);

pub async fn connector(config: TlsConfig<'_>) -> crate::Result<TlsConnector> {
    #[cfg(feature = "_tls-native-tls")]
    return tls_native_tls::connector(config).await;

    #[cfg(all(feature = "_tls-rustls", not(feature = "_tls-native-tls")))]
    return tls_rustls::connector(config).await;

    #[cfg(not(any(feature = "_tls-native-tls", feature = "_tls-rustls")))]
    {
        _ = config;
        panic!("one of the `runtime-*-native-tls` or `runtime-*-rustls` features must be enabled")
    }
}

/// Returns a connector that uses a configuration prepared by the application.
pub fn connector_from_prebuilt(config: &PrebuiltTlsConfig) -> TlsConnector {
    // This function never fails at runtime. Without the `Rustls` variant, this match has no arms
    // and cannot be reached. With the `IntoPrebuiltTlsConfig` impls compiled out too, no
    // `PrebuiltTlsConfig` can be constructed and a caller passing a Rustls configuration gets
    // the trait's `on_unimplemented` compile error at its own call site instead.
    match config.0 {
        #[cfg(all(feature = "_tls-rustls", not(feature = "_tls-native-tls")))]
        PrebuiltTlsConfigInner::Rustls(ref config) => tls_rustls::RustlsConnector {
            config: std::sync::Arc::clone(config),
        },
    }
}

pub async fn handshake<S, Ws>(
    socket: S,
    hostname: &str,
    connector: &TlsConnector,
    with_socket: Ws,
) -> crate::Result<Ws::Output>
where
    S: Socket,
    Ws: WithSocket,
{
    #[cfg(feature = "_tls-native-tls")]
    return Ok(with_socket
        .with_socket(tls_native_tls::handshake(socket, hostname, connector).await?)
        .await);

    #[cfg(all(feature = "_tls-rustls", not(feature = "_tls-native-tls")))]
    return Ok(with_socket
        .with_socket(tls_rustls::handshake(socket, hostname, connector).await?)
        .await);

    #[cfg(not(any(feature = "_tls-native-tls", feature = "_tls-rustls")))]
    {
        drop((socket, hostname, with_socket));
        match connector.0 {}
    }
}

pub fn available() -> bool {
    cfg!(any(feature = "_tls-native-tls", feature = "_tls-rustls"))
}

pub fn error_if_unavailable() -> crate::Result<()> {
    if !available() {
        return Err(Error::tls(
            "TLS upgrade required by connect options \
                    but SQLx was built without TLS support enabled",
        ));
    }

    Ok(())
}
