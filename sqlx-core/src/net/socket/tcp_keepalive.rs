use std::time::Duration;

/// TCP keepalive parameters for a connection's socket.
///
/// Keepalive is what lets a client notice that its server is gone when the
/// server disappeared without closing the socket: a failover, a killed
/// container, a NAT table that dropped the mapping. Without it, a connection
/// blocked reading a response it will never receive waits forever, because
/// there is nothing left to retransmit and so nothing to time out.
///
/// The parameters mirror libpq's `keepalives_idle`, `keepalives_interval` and
/// `keepalives_count`, and are applied with `setsockopt` after connecting. As in
/// libpq, a parameter that is unset or zero leaves the system default in place,
/// and durations have whole-second granularity: a fractional value is rounded up
/// to the next whole second.
///
/// # Platform support
///
/// `idle` sets `TCP_KEEPIDLE` (`TCP_KEEPALIVE` on Apple platforms), which exists
/// everywhere except OpenBSD, Haiku and PlayStation Vita, where it is silently
/// ignored. `interval` and `retries` set `TCP_KEEPINTVL` and `TCP_KEEPCNT`, which
/// exist on Linux, Android, Apple platforms, FreeBSD, NetBSD, DragonFly BSD,
/// illumos, Fuchsia, Cygwin and Windows, and are silently ignored elsewhere.
///
/// Windows sets `idle` and `interval` together through `SIO_KEEPALIVE_VALS` and
/// cannot leave one of them untouched, so an unset one gets the same substitute
/// libpq uses there: 2 hours idle, 1 second interval. `retries` maps to
/// `TCP_KEEPCNT`, which on Windows needs Windows 10 1703 or later.
///
/// A value the operating system rejects (Linux, for instance, caps `retries` at
/// 127 and `idle`/`interval` at 32767 seconds) fails the connection attempt with
/// [`Error::Configuration`](crate::Error::Configuration).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct TcpKeepalive {
    /// Idle time after which the first keepalive probe is sent, or `None` for the
    /// system default.
    pub idle: Option<Duration>,
    /// Time between probes once the first one has been sent, or `None` for the
    /// system default.
    pub interval: Option<Duration>,
    /// Number of unacknowledged probes before the connection is dropped, or `None`
    /// for the system default.
    pub retries: Option<u32>,
}

impl TcpKeepalive {
    /// Keepalive with no parameters overridden, leaving the system defaults in
    /// place. On Linux those are 7200s idle, 75s interval, 9 retries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the idle time after which the first keepalive probe is sent.
    ///
    /// Rounded up to whole seconds; zero leaves the system default in place.
    pub fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = whole_seconds(idle);
        self
    }

    /// Sets the time between keepalive probes.
    ///
    /// Rounded up to whole seconds; zero leaves the system default in place.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = whole_seconds(interval);
        self
    }

    /// Sets the number of unacknowledged probes before the connection is dropped.
    ///
    /// Zero leaves the system default in place.
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = (retries != 0).then_some(retries);
        self
    }

    // Matches the `cfg` the `socket2` dependency itself is declared under, so this is
    // only compiled where the crate is actually in the dependency graph.
    #[cfg(all(
        any(unix, windows),
        any(feature = "_rt-tokio", feature = "_rt-async-io")
    ))]
    pub(crate) fn to_socket2(self) -> socket2::TcpKeepalive {
        // Normalized again here rather than trusting the builders: the fields are
        // public, so they may have been assigned directly.
        let idle = self.idle.and_then(whole_seconds);

        // Windows cannot leave idle or interval unset (see the type docs); these are
        // libpq's substitutes, which are also Windows' own defaults.
        #[cfg(windows)]
        let idle = Some(idle.unwrap_or(Duration::from_secs(2 * 60 * 60)));

        let mut keepalive = socket2::TcpKeepalive::new();

        if let Some(idle) = idle {
            keepalive = keepalive.with_time(idle);
        }

        // `socket2` only exposes `with_interval`/`with_retries` on the platforms where
        // `TCP_KEEPINTVL`/`TCP_KEEPCNT` (or the Windows equivalents) exist, so this list
        // has to match its `cfg`s rather than exclude the platforms we know are missing:
        // calling them anywhere else is a compile error, not a runtime no-op. This is the
        // list from socket2 0.6.0, our minimum; later 0.6.x releases add a few more
        // targets (Emscripten, NuttX, WASI) that we leave out to keep the floor working.
        #[cfg(any(
            target_os = "android",
            target_os = "cygwin",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "fuchsia",
            target_os = "illumos",
            target_os = "ios",
            target_os = "linux",
            target_os = "macos",
            target_os = "netbsd",
            target_os = "tvos",
            target_os = "visionos",
            target_os = "watchos",
            target_os = "windows",
        ))]
        {
            let interval = self.interval.and_then(whole_seconds);

            #[cfg(windows)]
            let interval = Some(interval.unwrap_or(Duration::from_secs(1)));

            if let Some(interval) = interval {
                keepalive = keepalive.with_interval(interval);
            }

            if let Some(retries) = self.retries.filter(|&retries| retries != 0) {
                keepalive = keepalive.with_retries(retries);
            }
        }

        keepalive
    }
}

/// Rounds a duration up to whole seconds, the granularity of the keepalive socket
/// options: `socket2` truncates, which would turn anything under a second into a
/// literal 0, and Linux rejects that with `EINVAL`. Zero means "system default"
/// and becomes `None`.
fn whole_seconds(duration: Duration) -> Option<Duration> {
    let secs = duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0));

    (secs != 0).then(|| Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_overrides_nothing() {
        assert_eq!(
            TcpKeepalive::new(),
            TcpKeepalive {
                idle: None,
                interval: None,
                retries: None,
            }
        );
    }

    #[test]
    fn builders_set_each_parameter() {
        let keepalive = TcpKeepalive::new()
            .with_idle(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10))
            .with_retries(3);

        assert_eq!(keepalive.idle, Some(Duration::from_secs(30)));
        assert_eq!(keepalive.interval, Some(Duration::from_secs(10)));
        assert_eq!(keepalive.retries, Some(3));
    }

    #[test]
    fn zero_means_system_default() {
        let keepalive = TcpKeepalive::new()
            .with_idle(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10))
            .with_retries(3)
            .with_idle(Duration::ZERO)
            .with_interval(Duration::ZERO)
            .with_retries(0);

        assert_eq!(keepalive, TcpKeepalive::new());
    }

    #[test]
    fn fractional_durations_round_up() {
        let keepalive = TcpKeepalive::new()
            .with_idle(Duration::from_millis(500))
            .with_interval(Duration::from_millis(1500));

        assert_eq!(keepalive.idle, Some(Duration::from_secs(1)));
        assert_eq!(keepalive.interval, Some(Duration::from_secs(2)));
    }
}
