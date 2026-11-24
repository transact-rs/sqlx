use crate::any::AnyConnection;
use crate::connection::{ConnectOptions, LogSettings};
use crate::error::Error;
use log::LevelFilter;
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;
use url::Url;

/// Opaque options for connecting to a database. These may only be constructed by parsing from
/// a connection url.
///
/// ```text
/// postgres://postgres:password@localhost/database
/// mysql://root:password@localhost/database
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AnyConnectOptions {
    pub database_url: Url,
    pub log_settings: LogSettings,

    /// A custom [`rustls::ClientConfig`] for TLS connections.
    ///
    /// For details, see the corresponding setter on `MySqlConnectOptions` or `PgConnectOptions`,
    /// respectively.
    ///
    /// May not be applicable to some drivers (e.g. SQLite).
    #[cfg(feature = "_tls-rustls")]
    pub rustls_config: Option<std::sync::Arc<rustls::ClientConfig>>,
}
impl FromStr for AnyConnectOptions {
    type Err = Error;

    fn from_str(url: &str) -> Result<Self, Self::Err> {
        Ok(AnyConnectOptions {
            database_url: url
                .parse::<Url>()
                .map_err(|e| Error::Configuration(e.into()))?,
            log_settings: LogSettings::default(),
            #[cfg(feature = "_tls-rustls")]
            rustls_config: None,
        })
    }
}

impl ConnectOptions for AnyConnectOptions {
    type Connection = AnyConnection;

    fn from_url(url: &Url) -> Result<Self, Error> {
        Ok(AnyConnectOptions {
            database_url: url.clone(),
            log_settings: LogSettings::default(),
            #[cfg(feature = "_tls-rustls")]
            rustls_config: None,
        })
    }

    fn to_url_lossy(&self) -> Url {
        self.database_url.clone()
    }

    #[inline]
    fn connect(&self) -> impl Future<Output = Result<AnyConnection, Error>> + Send + '_ {
        AnyConnection::connect(self)
    }

    fn log_statements(mut self, level: LevelFilter) -> Self {
        self.log_settings.statements_level = level;
        self
    }

    fn log_slow_statements(mut self, level: LevelFilter, duration: Duration) -> Self {
        self.log_settings.slow_statements_level = level;
        self.log_settings.slow_statements_duration = duration;
        self
    }
}

impl AnyConnectOptions {}
