use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

pub use buffered::{BufferedSocket, WriteBuffer};
use bytes::BufMut;
use cfg_if::cfg_if;
pub use tcp_keepalive::TcpKeepalive;

use crate::io::ReadBuf;

mod buffered;
mod tcp_keepalive;

pub trait Socket: Send + Sync + Unpin + 'static {
    fn try_read(&mut self, buf: &mut dyn ReadBuf) -> io::Result<usize>;

    fn try_write(&mut self, buf: &[u8]) -> io::Result<usize>;

    fn poll_read_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    fn poll_write_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    fn poll_flush(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // `flush()` is a no-op for TCP/UDS
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    fn read<'a, B: ReadBuf>(&'a mut self, buf: &'a mut B) -> Read<'a, Self, B>
    where
        Self: Sized,
    {
        Read { socket: self, buf }
    }

    fn write<'a>(&'a mut self, buf: &'a [u8]) -> Write<'a, Self>
    where
        Self: Sized,
    {
        Write { socket: self, buf }
    }

    fn flush(&mut self) -> Flush<'_, Self>
    where
        Self: Sized,
    {
        Flush { socket: self }
    }

    fn shutdown(&mut self) -> Shutdown<'_, Self>
    where
        Self: Sized,
    {
        Shutdown { socket: self }
    }
}

pub struct Read<'a, S: ?Sized, B> {
    socket: &'a mut S,
    buf: &'a mut B,
}

impl<S: ?Sized, B> Future for Read<'_, S, B>
where
    S: Socket,
    B: ReadBuf,
{
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        while this.buf.has_remaining_mut() {
            match this.socket.try_read(&mut *this.buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(this.socket.poll_read_ready(cx))?;
                }
                ready => return Poll::Ready(ready),
            }
        }

        Poll::Ready(Ok(0))
    }
}

pub struct Write<'a, S: ?Sized> {
    socket: &'a mut S,
    buf: &'a [u8],
}

impl<S: ?Sized> Future for Write<'_, S>
where
    S: Socket,
{
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        while !this.buf.is_empty() {
            match this.socket.try_write(this.buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(this.socket.poll_write_ready(cx))?;
                }
                ready => return Poll::Ready(ready),
            }
        }

        Poll::Ready(Ok(0))
    }
}

pub struct Flush<'a, S: ?Sized> {
    socket: &'a mut S,
}

impl<S: Socket + ?Sized> Future for Flush<'_, S> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.socket.poll_flush(cx)
    }
}

pub struct Shutdown<'a, S: ?Sized> {
    socket: &'a mut S,
}

impl<S: ?Sized> Future for Shutdown<'_, S>
where
    S: Socket,
{
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.socket.poll_shutdown(cx)
    }
}

pub trait WithSocket {
    type Output;

    fn with_socket<S: Socket>(self, socket: S) -> impl Future<Output = Self::Output> + Send;
}

pub struct SocketIntoBox;

impl WithSocket for SocketIntoBox {
    type Output = Box<dyn Socket>;

    async fn with_socket<S: Socket>(self, socket: S) -> Self::Output {
        Box::new(socket)
    }
}

impl<S: Socket + ?Sized> Socket for Box<S> {
    fn try_read(&mut self, buf: &mut dyn ReadBuf) -> io::Result<usize> {
        (**self).try_read(buf)
    }

    fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (**self).try_write(buf)
    }

    fn poll_read_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        (**self).poll_read_ready(cx)
    }

    fn poll_write_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        (**self).poll_write_ready(cx)
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        (**self).poll_flush(cx)
    }

    fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        (**self).poll_shutdown(cx)
    }
}

/// Options applied to a TCP socket as it is opened.
///
/// `TCP_NODELAY` is not represented here: it is always set, see
/// [#4336](https://github.com/launchbadge/sqlx/pull/4336).
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct TcpConnectOptions {
    /// TCP keepalive parameters, or `None` to leave keepalive off.
    pub keepalive: Option<TcpKeepalive>,
}

impl TcpConnectOptions {
    /// Options that change nothing about the socket beyond the defaults SQLx always applies.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables TCP keepalive with the given parameters, or disables it if passed `None`.
    pub fn with_keepalive(mut self, keepalive: impl Into<Option<TcpKeepalive>>) -> Self {
        self.keepalive = keepalive.into();
        self
    }
}

pub async fn connect_tcp<Ws: WithSocket>(
    host: &str,
    port: u16,
    with_socket: Ws,
) -> crate::Result<Ws::Output> {
    connect_tcp_with(host, port, with_socket, &TcpConnectOptions::new()).await
}

/// Open a TCP socket to `host` and `port`, applying `options` to it.
///
/// The one option so far is keepalive. Without it, a connection whose server disappeared
/// without closing the socket (a failover, a killed container, a dropped NAT mapping) is
/// only discovered the next time the client writes to it, so a connection blocked reading
/// a response waits forever.
pub async fn connect_tcp_with<Ws: WithSocket>(
    host: &str,
    port: u16,
    with_socket: Ws,
    options: &TcpConnectOptions,
) -> crate::Result<Ws::Output> {
    #[cfg(feature = "_rt-tokio")]
    if crate::rt::rt_tokio::available() {
        let stream = tokio::net::TcpStream::connect((host, port)).await?;
        stream.set_nodelay(true)?;
        set_tcp_keepalive(&stream, options.keepalive)?;

        return Ok(with_socket.with_socket(stream).await);
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-io")] {
            // Options are applied inside, on the concrete socket type.
            Ok(with_socket.with_socket(connect_tcp_async_io(host, port, options).await?).await)
        } else {
            crate::rt::missing_rt((host, port, with_socket, options))
        }
    }
}

#[cfg(all(
    any(unix, windows),
    any(feature = "_rt-tokio", feature = "_rt-async-io")
))]
fn set_tcp_keepalive<'a>(
    stream: impl Into<socket2::SockRef<'a>>,
    keepalive: Option<TcpKeepalive>,
) -> crate::Result<()> {
    let Some(keepalive) = keepalive else {
        return Ok(());
    };

    stream
        .into()
        .set_tcp_keepalive(&keepalive.to_socket2())
        .map_err(|err| {
            crate::Error::Configuration(
                format!("failed to apply TCP keepalive settings {keepalive:?}: {err}").into(),
            )
        })
}

/// Applying keepalive needs `setsockopt()`, which this target does not have, so the
/// parameters are ignored rather than failing the connection.
#[cfg(all(
    not(any(unix, windows)),
    any(feature = "_rt-tokio", feature = "_rt-async-io")
))]
fn set_tcp_keepalive<S>(_stream: &S, _keepalive: Option<TcpKeepalive>) -> crate::Result<()> {
    Ok(())
}

/// Open a TCP socket to `host` and `port`.
///
/// If `host` is a hostname, attempt to connect to each address it resolves to.
///
/// This implements the same behavior as [`tokio::net::TcpStream::connect()`], additionally
/// sets the `TCP_NODELAY` flag, and applies `options`.
#[cfg(feature = "_rt-async-io")]
async fn connect_tcp_async_io(
    host: &str,
    port: u16,
    options: &TcpConnectOptions,
) -> crate::Result<impl Socket> {
    use async_io::Async;
    use std::net::{IpAddr, TcpStream, ToSocketAddrs};

    // IPv6 addresses in URLs will be wrapped in brackets and the `url` crate doesn't trim those.
    let host = host.trim_matches(&['[', ']'][..]);

    if let Ok(addr) = host.parse::<IpAddr>() {
        let stream = Async::<TcpStream>::connect((addr, port)).await?;
        stream.get_ref().set_nodelay(true)?;
        set_tcp_keepalive(&stream, options.keepalive)?;

        return Ok(stream);
    }

    let host = host.to_string();

    let addresses = crate::rt::spawn_blocking(move || {
        let addr = (host.as_str(), port);
        ToSocketAddrs::to_socket_addrs(&addr)
    })
    .await?;

    let mut last_err = None;

    // Loop through all the Socket Addresses that the hostname resolves to
    for socket_addr in addresses {
        match Async::<TcpStream>::connect(socket_addr).await {
            Ok(stream) => {
                stream.get_ref().set_nodelay(true)?;
                set_tcp_keepalive(&stream, options.keepalive)?;

                return Ok(stream);
            }
            Err(e) => last_err = Some(e),
        }
    }

    // If we reach this point, it means we failed to connect to any of the addresses.
    // Return the last error we encountered, or a custom error if the hostname didn't resolve to any address.
    Err(last_err
        .unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "Hostname did not resolve to any addresses",
            )
        })
        .into())
}

/// Connect a Unix Domain Socket at the given path.
///
/// Returns an error if Unix Domain Sockets are not supported on this platform.
pub async fn connect_uds<P: AsRef<Path>, Ws: WithSocket>(
    path: P,
    with_socket: Ws,
) -> crate::Result<Ws::Output> {
    #[cfg(unix)]
    {
        #[cfg(feature = "_rt-tokio")]
        if crate::rt::rt_tokio::available() {
            use tokio::net::UnixStream;

            let stream = UnixStream::connect(path).await?;

            return Ok(with_socket.with_socket(stream).await);
        }

        cfg_if! {
            if #[cfg(feature = "_rt-async-io")] {
                use async_io::Async;
                use std::os::unix::net::UnixStream;

                let stream = Async::<UnixStream>::connect(path).await?;

                Ok(with_socket.with_socket(stream).await)
            } else {
                crate::rt::missing_rt((path, with_socket))
            }
        }
    }

    #[cfg(not(unix))]
    {
        drop((path, with_socket));

        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix domain sockets are not supported on this platform",
        )
        .into())
    }
}

#[cfg(all(
    test,
    any(unix, windows),
    any(feature = "_rt-tokio", feature = "_rt-async-io")
))]
mod tests {
    use super::*;
    use socket2::SockRef;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    /// A connected TCP socket. The listener never accepts, which does not matter for
    /// setting socket options; it is returned so that it outlives the stream.
    fn loopback() -> (TcpListener, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();

        (listener, stream)
    }

    #[test]
    fn none_leaves_keepalive_off() {
        let (_listener, stream) = loopback();

        set_tcp_keepalive(&stream, None).unwrap();

        assert!(!SockRef::from(&stream).keepalive().unwrap());
    }

    #[test]
    fn parameters_reach_the_socket() {
        let (_listener, stream) = loopback();
        let keepalive = TcpKeepalive::new()
            .with_idle(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10))
            .with_retries(3);

        set_tcp_keepalive(&stream, Some(keepalive)).unwrap();

        let socket = SockRef::from(&stream);
        assert!(socket.keepalive().unwrap());

        // `socket2` only has getters for the individual parameters on some platforms;
        // these cover CI and local development.
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        {
            assert_eq!(
                socket.tcp_keepalive_time().unwrap(),
                Duration::from_secs(30)
            );
            assert_eq!(
                socket.tcp_keepalive_interval().unwrap(),
                Duration::from_secs(10)
            );
            assert_eq!(socket.tcp_keepalive_retries().unwrap(), 3);
        }
    }

    /// Linux rejects a literal 0 for any of the three options with `EINVAL`, and
    /// `socket2` truncates durations to whole seconds, so zero and sub-second values
    /// must never reach `setsockopt` as 0.
    ///
    /// Assigned to the fields directly rather than through the builders: the fields
    /// are public, so normalizing in the builders alone would not be enough.
    #[test]
    fn zero_and_sub_second_values_are_not_passed_through() {
        let (_listener, stream) = loopback();

        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        let system_defaults = {
            let socket = SockRef::from(&stream);

            (
                socket.tcp_keepalive_time().unwrap(),
                socket.tcp_keepalive_retries().unwrap(),
            )
        };

        let mut keepalive = TcpKeepalive::new();
        keepalive.idle = Some(Duration::ZERO);
        keepalive.interval = Some(Duration::from_millis(500));
        keepalive.retries = Some(0);

        set_tcp_keepalive(&stream, Some(keepalive)).unwrap();

        let socket = SockRef::from(&stream);
        assert!(socket.keepalive().unwrap());

        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        {
            assert_eq!(
                (
                    socket.tcp_keepalive_time().unwrap(),
                    socket.tcp_keepalive_retries().unwrap(),
                ),
                system_defaults,
            );
            assert_eq!(
                socket.tcp_keepalive_interval().unwrap(),
                Duration::from_secs(1)
            );
        }
    }

    /// A value the kernel rejects fails with an error that says what was being set.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn rejected_values_name_keepalive_in_the_error() {
        let (_listener, stream) = loopback();

        // Becomes -1 once cast to the `c_int` the option takes.
        let keepalive = TcpKeepalive::new().with_retries(u32::MAX);

        let err = set_tcp_keepalive(&stream, Some(keepalive)).unwrap_err();

        assert!(matches!(err, crate::Error::Configuration(_)), "{err:?}");
        assert!(err.to_string().contains("TCP keepalive"), "{err}");
    }
}
