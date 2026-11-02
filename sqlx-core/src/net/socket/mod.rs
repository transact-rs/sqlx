use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::time::Duration;

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
/// `TCP_NODELAY` is not represented here: it is always set.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct TcpConnectOptions {
    /// TCP keepalive parameters, or `None` to leave keepalive off.
    pub keepalive: Option<TcpKeepalive>,
    /// `TCP_USER_TIMEOUT`, or `None` to keep the operating system default.
    pub user_timeout: Option<Duration>,
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

    /// Sets `TCP_USER_TIMEOUT`: the kernel drops the connection once sent data, keepalive probes
    /// included, has gone unacknowledged for `timeout`. `None` or a zero duration keeps the
    /// operating system default. The kernel counts whole milliseconds, so a shorter non-zero
    /// `timeout` becomes one.
    ///
    /// Only Linux, Android, Fuchsia and Cygwin have the option; elsewhere it is ignored.
    pub fn with_user_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.user_timeout = timeout.into();
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
/// A connection whose server disappeared without closing the socket (a failover, a killed
/// container, a dropped NAT mapping) otherwise looks open until the operating system gives up on
/// it: never, while the client waits for a response, and after about 15 minutes of retransmission
/// on Linux, while it has data unacknowledged. Keepalive bounds the first case and
/// `TCP_USER_TIMEOUT` the second.
pub async fn connect_tcp_with<Ws: WithSocket>(
    host: &str,
    port: u16,
    with_socket: Ws,
    options: &TcpConnectOptions,
) -> crate::Result<Ws::Output> {
    #[cfg(feature = "_rt-tokio")]
    if crate::rt::rt_tokio::available() {
        let socket = tokio::net::TcpStream::connect((host, port)).await?;
        socket.set_nodelay(true)?;
        apply_tcp_options(&socket, options)?;
        return Ok(with_socket.with_socket(socket).await);
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
fn apply_tcp_options<'a>(
    stream: impl Into<socket2::SockRef<'a>>,
    options: &TcpConnectOptions,
) -> crate::Result<()> {
    let socket = stream.into();

    if let Some(keepalive) = options.keepalive {
        socket
            .set_tcp_keepalive(&keepalive.to_socket2())
            .map_err(|err| {
                crate::Error::Configuration(
                    format!("failed to apply TCP keepalive settings {keepalive:?}: {err}").into(),
                )
            })?;
    }

    // Only these targets have `TCP_USER_TIMEOUT`; elsewhere the option is ignored rather than
    // failing the connection.
    #[cfg(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "linux",
        target_os = "cygwin"
    ))]
    if let Some(timeout) = options.user_timeout {
        // socket2 truncates to whole milliseconds, and zero means the system default.
        let timeout = if timeout.is_zero() {
            timeout
        } else {
            std::cmp::max(timeout, Duration::from_millis(1))
        };

        socket.set_tcp_user_timeout(Some(timeout)).map_err(|err| {
            crate::Error::Configuration(
                format!("failed to set TCP_USER_TIMEOUT to {timeout:?}: {err}").into(),
            )
        })?;
    }

    Ok(())
}

/// Applying socket options needs `setsockopt()`, which this target does not have, so they are
/// ignored rather than failing the connection.
#[cfg(all(
    not(any(unix, windows)),
    any(feature = "_rt-tokio", feature = "_rt-async-io")
))]
fn apply_tcp_options<S>(_stream: &S, _options: &TcpConnectOptions) -> crate::Result<()> {
    Ok(())
}

/// Open a TCP socket to `host` and `port`.
///
/// If `host` is a hostname, attempt to connect to each address it resolves to.
///
/// This implements the same behavior as [`tokio::net::TcpStream::connect()`], additionally sets
/// the `TCP_NODELAY` flag, and applies `options`.
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
        let socket = Async::<TcpStream>::connect((addr, port)).await?;
        socket.get_ref().set_nodelay(true)?;
        apply_tcp_options(socket.get_ref(), options)?;
        return Ok(socket);
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
                apply_tcp_options(stream.get_ref(), options)?;
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

    fn loopback() -> (TcpListener, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();

        (listener, stream)
    }

    #[test]
    fn default_options_leave_the_socket_alone() {
        let (_listener, stream) = loopback();
        let socket = SockRef::from(&stream);
        #[cfg(target_os = "linux")]
        let user_timeout = socket.tcp_user_timeout().unwrap();

        apply_tcp_options(&stream, &TcpConnectOptions::new()).unwrap();

        assert!(!socket.keepalive().unwrap());
        #[cfg(target_os = "linux")]
        assert_eq!(socket.tcp_user_timeout().unwrap(), user_timeout);
    }

    #[test]
    fn keepalive_parameters_reach_the_socket() {
        let (_listener, stream) = loopback();
        let keepalive = TcpKeepalive::new()
            .with_idle(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10))
            .with_retries(3);

        apply_tcp_options(&stream, &TcpConnectOptions::new().with_keepalive(keepalive)).unwrap();

        let socket = SockRef::from(&stream);
        assert!(socket.keepalive().unwrap());

        // `socket2` only has getters for the individual parameters on some platforms; these
        // cover CI and local development.
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

    /// Linux rejects a literal 0 for any of the three options with `EINVAL`, and `socket2`
    /// truncates durations to whole seconds, so zero and sub-second values must never reach
    /// `setsockopt` as 0.
    ///
    /// Assigned to the fields directly rather than through the builders: the fields are public,
    /// so normalizing in the builders alone would not be enough.
    #[test]
    fn zero_and_sub_second_keepalive_values_are_not_passed_through() {
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

        apply_tcp_options(&stream, &TcpConnectOptions::new().with_keepalive(keepalive)).unwrap();

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
    fn rejected_keepalive_values_name_keepalive_in_the_error() {
        let (_listener, stream) = loopback();

        // Becomes -1 once cast to the `c_int` the option takes.
        let keepalive = TcpKeepalive::new().with_retries(u32::MAX);

        let err = apply_tcp_options(&stream, &TcpConnectOptions::new().with_keepalive(keepalive))
            .unwrap_err();

        assert!(matches!(err, crate::Error::Configuration(_)), "{err:?}");
        assert!(err.to_string().contains("TCP keepalive"), "{err}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn user_timeout_reaches_the_socket() {
        let (_listener, stream) = loopback();
        let socket = SockRef::from(&stream);

        let options = TcpConnectOptions::new().with_user_timeout(Duration::from_secs(60));
        apply_tcp_options(&stream, &options).unwrap();
        assert_eq!(
            socket.tcp_user_timeout().unwrap(),
            Some(Duration::from_secs(60))
        );

        let options = TcpConnectOptions::new().with_user_timeout(Duration::from_micros(500));
        apply_tcp_options(&stream, &options).unwrap();
        assert_eq!(
            socket.tcp_user_timeout().unwrap(),
            Some(Duration::from_millis(1)),
            "a sub-millisecond timeout must not truncate to the system default"
        );
    }

    #[cfg(feature = "_rt-tokio")]
    #[test]
    fn connect_tcp_with_applies_the_options() {
        struct AssertApplied;

        impl WithSocket for AssertApplied {
            type Output = ();

            async fn with_socket<S: Socket>(self, socket: S) -> Self::Output {
                let stream = (&socket as &dyn std::any::Any)
                    .downcast_ref::<tokio::net::TcpStream>()
                    .expect("the tokio path hands over a tokio TcpStream");
                let socket = SockRef::from(stream);

                assert!(socket.keepalive().unwrap());
                #[cfg(target_os = "linux")]
                assert_eq!(
                    socket.tcp_user_timeout().unwrap(),
                    Some(Duration::from_secs(45))
                );
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let options = TcpConnectOptions::new()
            .with_keepalive(TcpKeepalive::new().with_idle(Duration::from_secs(30)))
            .with_user_timeout(Duration::from_secs(45));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(connect_tcp_with("127.0.0.1", port, AssertApplied, &options))
            .unwrap();
    }
}
