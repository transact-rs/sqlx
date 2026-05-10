use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::time::Duration;

pub use buffered::{BufferedSocket, WriteBuffer};
use bytes::BufMut;
use cfg_if::cfg_if;

use crate::io::ReadBuf;

mod buffered;

/// Configuration for TCP keepalive probes on a connection.
///
/// All fields default to `None`, meaning the OS default is used.
/// Constructing a `KeepaliveConfig::default()` and passing it enables keepalive
/// with OS defaults for all parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeepaliveConfig {
    /// Time the connection must be idle before keepalive probes begin.
    /// `None` means the OS default.
    pub idle: Option<Duration>,
    /// Interval between keepalive probes.
    /// `None` means the OS default.
    pub interval: Option<Duration>,
    /// Maximum number of failed probes before the connection is dropped.
    /// Only supported on Unix; ignored on other platforms.
    /// `None` means the OS default.
    pub retries: Option<u32>,
}

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

#[cfg(any(feature = "_rt-tokio", feature = "_rt-async-io"))]
fn build_tcp_keepalive(config: &KeepaliveConfig) -> socket2::TcpKeepalive {
    let mut ka = socket2::TcpKeepalive::new();

    if let Some(idle) = config.idle {
        ka = ka.with_time(idle);
    }

    // socket2's `with_interval` is unavailable on these platforms.
    #[cfg(not(any(
        target_os = "haiku",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "solaris",
    )))]
    if let Some(interval) = config.interval {
        ka = ka.with_interval(interval);
    }

    // socket2's `with_retries` is unavailable on these platforms.
    #[cfg(not(any(
        target_os = "haiku",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "solaris",
        target_os = "windows",
    )))]
    if let Some(retries) = config.retries {
        ka = ka.with_retries(retries);
    }

    ka
}

pub async fn connect_tcp<Ws: WithSocket>(
    host: &str,
    port: u16,
    keepalive: Option<&KeepaliveConfig>,
    with_socket: Ws,
) -> crate::Result<Ws::Output> {
    #[cfg(feature = "_rt-tokio")]
    if crate::rt::rt_tokio::available() {
        let stream = tokio::net::TcpStream::connect((host, port)).await?;

        if let Some(ka) = keepalive {
            let sock = socket2::SockRef::from(&stream);
            sock.set_tcp_keepalive(&build_tcp_keepalive(ka))?;
        }

        return Ok(with_socket.with_socket(stream).await);
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-io")] {
            Ok(with_socket.with_socket(connect_tcp_async_io(host, port, keepalive).await?).await)
        } else {
            crate::rt::missing_rt((host, port, keepalive, with_socket))
        }
    }
}

/// Open a TCP socket to `host` and `port`.
///
/// If `host` is a hostname, attempt to connect to each address it resolves to.
///
/// This implements the same behavior as [`tokio::net::TcpStream::connect()`].
#[cfg(feature = "_rt-async-io")]
async fn connect_tcp_async_io(
    host: &str,
    port: u16,
    keepalive: Option<&KeepaliveConfig>,
) -> crate::Result<impl Socket> {
    use async_io::Async;
    use std::net::{IpAddr, TcpStream, ToSocketAddrs};

    // IPv6 addresses in URLs will be wrapped in brackets and the `url` crate doesn't trim those.
    let host = host.trim_matches(&['[', ']'][..]);

    if let Ok(addr) = host.parse::<IpAddr>() {
        let stream = Async::<TcpStream>::connect((addr, port)).await?;

        if let Some(ka) = keepalive {
            let sock = socket2::SockRef::from(stream.get_ref());
            sock.set_tcp_keepalive(&build_tcp_keepalive(ka))?;
        }

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
                if let Some(ka) = keepalive {
                    let sock = socket2::SockRef::from(stream.get_ref());
                    sock.set_tcp_keepalive(&build_tcp_keepalive(ka))?;
                }

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
