use std::io;

use crate::error::Error;
use crate::net::tls::{self, TlsConfig};
use crate::net::{Socket, SocketIntoBox, WithSocket};

use crate::message::SslRequest;
use crate::{PgConnectOptions, PgSslMode};

pub struct MaybeUpgradeTls<'a>(pub &'a PgConnectOptions);

impl WithSocket for MaybeUpgradeTls<'_> {
    type Output = crate::Result<Box<dyn Socket>>;

    async fn with_socket<S: Socket>(self, socket: S) -> Self::Output {
        maybe_upgrade(socket, self.0).await
    }
}

async fn maybe_upgrade<S: Socket>(
    mut socket: S,
    options: &PgConnectOptions,
) -> Result<Box<dyn Socket>, Error> {
    // https://www.postgresql.org/docs/12/libpq-ssl.html#LIBPQ-SSL-SSLMODE-STATEMENTS
    match options.ssl_options.ssl_mode {
        // FIXME: Implement ALLOW
        PgSslMode::Allow | PgSslMode::Disable => return Ok(Box::new(socket)),

        PgSslMode::Prefer => {
            if !tls::available() {
                return Ok(Box::new(socket));
            }

            // try upgrade, but its okay if we fail
            if !request_upgrade(&mut socket, options).await? {
                return Ok(Box::new(socket));
            }
        }

        PgSslMode::Require | PgSslMode::VerifyFull | PgSslMode::VerifyCa => {
            tls::error_if_unavailable()?;

            if !request_upgrade(&mut socket, options).await? {
                // upgrade failed, die
                return Err(Error::Tls("server does not support TLS".into()));
            }
        }
    }

    let connector = if let Some(c) = options.ssl_options.cached_connector.get() {
        c
    } else {
        let accept_invalid_certs = !matches!(
            options.ssl_options.ssl_mode,
            PgSslMode::VerifyCa | PgSslMode::VerifyFull
        );
        let accept_invalid_hostnames =
            !matches!(options.ssl_options.ssl_mode, PgSslMode::VerifyFull);

        let config = TlsConfig {
            accept_invalid_certs,
            accept_invalid_hostnames,
            root_cert_path: options.ssl_options.ssl_root_cert.as_ref(),
            client_cert_path: options.ssl_options.ssl_client_cert.as_ref(),
            client_key_path: options.ssl_options.ssl_client_key.as_ref(),
        };
        let connector = tls::connector(config).await?;
        options
            .ssl_options
            .cached_connector
            .get_or_init(|| connector)
    };

    tls::handshake(socket, &options.host, connector, SocketIntoBox).await
}

async fn request_upgrade(
    socket: &mut impl Socket,
    _options: &PgConnectOptions,
) -> Result<bool, Error> {
    // https://www.postgresql.org/docs/current/protocol-flow.html#id-1.10.5.7.11

    // To initiate an SSL-encrypted connection, the frontend initially sends an
    // SSLRequest message rather than a StartupMessage

    socket.write(SslRequest::BYTES).await?;

    // The server then responds with a single byte containing S or N, indicating that
    // it is willing or unwilling to perform SSL, respectively.

    let mut response = [0u8];

    let n = socket.read(&mut &mut response[..]).await?;
    if n == 0 {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
    }

    match response[0] {
        b'S' => {
            // The server is ready and willing to accept an SSL connection
            Ok(true)
        }

        b'N' => {
            // The server is _unwilling_ to perform SSL
            Ok(false)
        }

        other => Err(err_protocol!(
            "unexpected response from SSLRequest: 0x{:02x}",
            other
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx_core::io::ReadBuf;
    use std::io;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    struct MockSocket {
        read_byte: Option<u8>,
    }

    impl Socket for MockSocket {
        fn try_read(&mut self, buf: &mut dyn ReadBuf) -> io::Result<usize> {
            match self.read_byte {
                Some(b) => {
                    buf.put_slice(&[b]);
                    Ok(1)
                }
                None => Ok(0), // EOF
            }
        }

        fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn poll_read_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_write_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Polls a future that is guaranteed to complete immediately on its first poll.
    ///
    /// This safe test runner uses `Waker::noop()` and `std::pin::pin!` without needing
    /// a full runtime or unsafe waker implementations.
    fn poll_immediate<F: std::future::Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        let mut cx = Context::from_waker(Waker::noop());
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(val) => val,
            Poll::Pending => panic!("test future was pending unexpectedly"),
        }
    }

    #[test]
    fn test_request_upgrade_eof() {
        poll_immediate(async {
            let mut socket = MockSocket { read_byte: None };
            let options = PgConnectOptions::new();
            let err = request_upgrade(&mut socket, &options).await.unwrap_err();

            match err {
                Error::Io(io_err) => {
                    assert_eq!(io_err.kind(), io::ErrorKind::UnexpectedEof);
                }
                other => panic!("expected Error::Io(UnexpectedEof), but got: {:?}", other),
            }
        });
    }

    #[test]
    fn test_request_upgrade_wire_null_byte() {
        poll_immediate(async {
            let mut socket = MockSocket {
                read_byte: Some(0x00),
            };
            let options = PgConnectOptions::new();
            let err = request_upgrade(&mut socket, &options).await.unwrap_err();

            match err {
                Error::Protocol(msg) => {
                    assert!(msg.contains("unexpected response from SSLRequest: 0x00"));
                }
                other => panic!("expected Error::Protocol, but got: {:?}", other),
            }
        });
    }

    #[test]
    fn test_request_upgrade_supported() {
        poll_immediate(async {
            let mut socket = MockSocket {
                read_byte: Some(b'S'),
            };
            let options = PgConnectOptions::new();
            let res = request_upgrade(&mut socket, &options).await.unwrap();
            assert!(res);
        });
    }

    #[test]
    fn test_request_upgrade_unsupported() {
        poll_immediate(async {
            let mut socket = MockSocket {
                read_byte: Some(b'N'),
            };
            let options = PgConnectOptions::new();
            let res = request_upgrade(&mut socket, &options).await.unwrap();
            assert!(!res);
        });
    }

    #[test]
    fn test_request_upgrade_unexpected_byte() {
        poll_immediate(async {
            let mut socket = MockSocket {
                read_byte: Some(0x42),
            };
            let options = PgConnectOptions::new();
            let err = request_upgrade(&mut socket, &options).await.unwrap_err();

            match err {
                Error::Protocol(msg) => {
                    assert!(msg.contains("unexpected response from SSLRequest: 0x42"));
                }
                other => panic!("expected Error::Protocol, but got: {:?}", other),
            }
        });
    }
}
