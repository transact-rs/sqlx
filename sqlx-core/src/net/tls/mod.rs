#![allow(dead_code)]

use crate::error::Error;
use crate::net::socket::WithSocket;
use crate::net::Socket;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

#[cfg(feature = "_tls-rustls")]
mod tls_rustls;

#[cfg(feature = "_tls-native-tls")]
mod tls_native_tls;

#[cfg(feature = "_tls-rustls")]
pub use tls_rustls::RustlsConnector;

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

pub enum TlsConfig<'a> {
    Options(TlsOptions<'a>),
    #[cfg(feature = "_tls-rustls")]
    PrebuiltRustls(&'a Arc<rustls::ClientConfig>),
}

pub struct TlsOptions<'a> {
    pub accept_invalid_certs: bool,
    pub accept_invalid_hostnames: bool,
    pub root_cert_path: Option<&'a CertificateInput>,
    pub client_cert_path: Option<&'a CertificateInput>,
    pub client_key_path: Option<&'a CertificateInput>,

    #[cfg(feature = "_tls-rustls")]
    pub rustls_crypto_provider: Option<&'a Arc<rustls::crypto::CryptoProvider>>,
}

#[cfg(feature = "_tls-rustls")]
impl<'a> From<&'a Arc<rustls::ClientConfig>> for TlsConfig<'a> {
    fn from(value: &'a Arc<rustls::ClientConfig>) -> Self {
        TlsConfig::PrebuiltRustls(value)
    }
}

impl<'a> From<TlsOptions<'a>> for TlsConfig<'a> {
    fn from(value: TlsOptions<'a>) -> Self {
        TlsConfig::Options(value)
    }
}

#[derive(Debug, Clone)]
pub enum TlsConnector {
    #[cfg(feature = "_tls-native-tls")]
    NativeTls(tls_native_tls::NativeTlsConnector),
    #[cfg(feature = "_tls-rustls")]
    Rustls(RustlsConnector),

    #[cfg(not(any(feature = "_tls-native-tls", feature = "_tls-rustls")))]
    Noop,
}

#[cfg(feature = "_tls-rustls")]
impl From<RustlsConnector> for TlsConnector {
    fn from(value: RustlsConnector) -> Self {
        Self::Rustls(value)
    }
}

#[derive(Debug, Default, Clone)]
pub struct WithCachedTlsConnector<Opts> {
    options: Opts,
    connector: Arc<OnceLock<TlsConnector>>,
}

impl<Opts> WithCachedTlsConnector<Opts> {
    pub fn new(options: Opts) -> Self {
        Self {
            options,
            connector: Arc::new(OnceLock::new()),
        }
    }

    pub fn options(&self) -> &Opts {
        &self.options
    }

    /// Invalidate the cached connector and mutate the options.
    pub fn options_mut(&mut self) -> &mut Opts {
        self.connector = Arc::new(OnceLock::new());
        &mut self.options
    }

    pub fn get_connector(&self) -> Option<&TlsConnector> {
        self.connector.get()
    }

    pub fn get_connector_or_init(&self, init: impl FnOnce() -> TlsConnector) -> &TlsConnector {
        self.connector.get_or_init(init)
    }

    pub fn set_connector(&mut self, connector: TlsConnector) {
        *Arc::make_mut(&mut self.connector) = connector.into();
    }
}

pub async fn connector(config: TlsConfig<'_>) -> crate::Result<TlsConnector> {
    // New behavior: `rustls` takes precedent if an explicit `rustls::ClientConfig` is set
    #[cfg(feature = "_tls-native-tls")]
    if let TlsConfig::Options(options) = config {
        return Ok(TlsConnector::NativeTls(
            tls_native_tls::connector(options).await?,
        ));
    }

    #[cfg(feature = "_tls-rustls")]
    return Ok(TlsConnector::Rustls(tls_rustls::connector(config).await?));

    #[cfg(not(any(feature = "_tls-native-tls", feature = "_tls-rustls")))]
    {
        let _ = config;
        Ok(TlsConnector::Noop)
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
    // If only one backend is enabled, this `match` should be a no-op.
    match connector {
        #[cfg(feature = "_tls-native-tls")]
        TlsConnector::NativeTls(connector) => Ok(with_socket
            .with_socket(tls_native_tls::handshake(socket, hostname, connector).await?)
            .await),
        #[cfg(feature = "_tls-rustls")]
        TlsConnector::Rustls(connector) => Ok(with_socket
            .with_socket(tls_rustls::handshake(socket, hostname, connector).await?)
            .await),
        #[cfg(not(any(feature = "_tls-native-tls", feature = "_tls-rustls")))]
        TlsConnector::Noop => {
            drop((socket, hostname, with_socket));
            Err(Error::config("SQLx driver expected TLS upgrade but no TLS feature was enabled (`tls-native-tls` or `tls-rustls`)"))
        }
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
