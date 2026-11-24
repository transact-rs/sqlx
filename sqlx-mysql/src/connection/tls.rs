use crate::connection::{MySqlStream, Waiting};
use crate::error::Error;
use crate::net::tls::TlsOptions;
use crate::net::{tls, BufferedSocket, Socket, WithSocket};
use crate::protocol::connect::SslRequest;
use crate::protocol::Capabilities;
use crate::{MySqlConnectOptions, MySqlSslMode};
use std::collections::VecDeque;

struct MapStream {
    server_version: (u16, u16, u16),
    capabilities: Capabilities,
    sequence_id: u8,
    waiting: VecDeque<Waiting>,
}

pub(super) async fn maybe_upgrade<S: Socket>(
    mut stream: MySqlStream<S>,
    options: &MySqlConnectOptions,
) -> Result<MySqlStream, Error> {
    let server_supports_tls = stream.capabilities.contains(Capabilities::SSL);

    // https://www.postgresql.org/docs/12/libpq-ssl.html#LIBPQ-SSL-SSLMODE-STATEMENTS
    match options.ssl.options().ssl_mode {
        MySqlSslMode::Disabled => {
            // remove the SSL capability if SSL has been explicitly disabled
            stream.capabilities.remove(Capabilities::SSL);
            return Ok(stream.boxed_socket());
        }

        MySqlSslMode::Preferred => {
            if !tls::available() {
                // Client doesn't support TLS
                tracing::debug!("not performing TLS upgrade: TLS support not compiled in");
                // don't advertise the capability if not available
                stream.capabilities.remove(Capabilities::SSL);
                return Ok(stream.boxed_socket());
            }

            if !server_supports_tls {
                // Server doesn't support TLS
                tracing::debug!("not performing TLS upgrade: unsupported by server");
                return Ok(stream.boxed_socket());
            }
        }

        MySqlSslMode::Required | MySqlSslMode::VerifyIdentity | MySqlSslMode::VerifyCa => {
            tls::error_if_unavailable()?;

            if !server_supports_tls {
                // upgrade failed, die
                return Err(Error::Tls("server does not support TLS".into()));
            }
        }
    }

    let connector = if let Some(c) = options.ssl.get_connector() {
        c
    } else {
        let ssl_options = options.ssl.options();

        let tls_config = TlsOptions {
            accept_invalid_certs: !matches!(
                ssl_options.ssl_mode,
                MySqlSslMode::VerifyCa | MySqlSslMode::VerifyIdentity
            ),
            accept_invalid_hostnames: !matches!(ssl_options.ssl_mode, MySqlSslMode::VerifyIdentity),
            root_cert_path: ssl_options.ssl_ca.as_ref(),
            client_cert_path: ssl_options.ssl_client_cert.as_ref(),
            client_key_path: ssl_options.ssl_client_key.as_ref(),

            #[cfg(feature = "_tls-rustls")]
            rustls_crypto_provider: None,
        };
        let connector = tls::connector(tls_config.into()).await?;
        options.ssl.get_connector_or_init(|| connector)
    };

    // Request TLS upgrade
    stream.write_packet(SslRequest {
        max_packet_size: super::MAX_PACKET_SIZE,
        charset: super::INITIAL_CHARSET,
    })?;

    stream.flush().await?;

    tls::handshake(
        stream.socket.into_inner(),
        options
            .tls_sni_servername
            .as_deref()
            .unwrap_or_else(|| options.host.stripped()),
        connector,
        MapStream {
            server_version: stream.server_version,
            capabilities: stream.capabilities,
            sequence_id: stream.sequence_id,
            waiting: stream.waiting,
        },
    )
    .await
}

impl WithSocket for MapStream {
    type Output = MySqlStream;

    async fn with_socket<S: Socket>(self, socket: S) -> Self::Output {
        MySqlStream {
            socket: BufferedSocket::new(Box::new(socket)),
            server_version: self.server_version,
            capabilities: self.capabilities,
            sequence_id: self.sequence_id,
            waiting: self.waiting,
            is_tls: true,
        }
    }
}
