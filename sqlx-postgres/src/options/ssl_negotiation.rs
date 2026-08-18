use crate::error::Error;
use std::str::FromStr;

/// Options for controlling the connection establishment procedure for PostgreSQL SSL connections.
///
/// It is used by the [`sslnegotiation`](super::PgConnectOptions::ssl_negotiation) method.
#[derive(Debug, Clone, Copy, Default)]
pub enum PgSslNegotiation {
    /// The client first asks the server if SSL is supported.
    ///
    /// This is the default if no other mode is specified.
    #[default]
    Postgres,

    /// The client starts the standard SSL handshake directly after establishing the TCP/IP
    /// connection.
    Direct,
}

impl FromStr for PgSslNegotiation {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        Ok(match &*s.to_ascii_lowercase() {
            "postgres" => PgSslNegotiation::Postgres,
            "direct" => PgSslNegotiation::Direct,

            _ => {
                return Err(Error::Configuration(
                    format!("unknown value {s:?} for `ssl_negotiation`").into(),
                ));
            }
        })
    }
}
