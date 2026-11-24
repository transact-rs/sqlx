mod socket;
pub mod tls;

pub use socket::{
    connect_tcp, connect_uds, BufferedSocket, Socket, SocketIntoBox, WithSocket, WriteBuffer,
};
use std::fmt::{Debug, Display, Formatter};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, PartialEq, Eq)]
pub struct UrlHost {
    original: Box<str>,
    brackets: Brackets,
}

#[derive(Clone, PartialEq, Eq)]
enum Brackets {
    None,
    Included,
    Needed,
}

impl UrlHost {
    pub fn empty() -> Self {
        UrlHost {
            original: "".into(),
            brackets: Brackets::None,
        }
    }

    pub fn localhost() -> Self {
        UrlHost {
            original: "localhost".into(),
            brackets: Brackets::None,
        }
    }

    fn check_brackets(&mut self) {
        if self.original.starts_with('[') && self.original.ends_with(']') {
            self.brackets = Brackets::Included;
        } else if self.original.parse::<Ipv6Addr>().is_ok() {
            // IPv6 address passed through `.host()`, should be bracketed in a URL
            self.brackets = Brackets::Needed;
        } else {
            self.brackets = Brackets::None;
        }
    }

    pub fn set(&mut self, s: &str) {
        self.original = s.into();
        self.check_brackets();
    }

    pub fn original(&self) -> &str {
        &self.original
    }

    pub fn stripped(&self) -> &str {
        self.original.trim_matches(&['[', ']'][..])
    }
}

impl From<String> for UrlHost {
    fn from(value: String) -> Self {
        let mut this = Self {
            original: value.into(),
            brackets: Brackets::None,
        };
        this.check_brackets();
        this
    }
}

impl Debug for UrlHost {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.original, f)
    }
}

impl Display for UrlHost {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Brackets::Needed = self.brackets {
            write!(f, "[{}]", self.original)
        } else {
            f.write_str(&self.original)
        }
    }
}

impl<'a> From<&'a UrlHost> for SocketHost<'a> {
    fn from(host: &'a UrlHost) -> Self {
        SocketHost::Str(host.stripped())
    }
}

#[derive(Clone, Copy)]
pub enum SocketHost<'a> {
    Str(&'a str),
    // Separating `Ipv4Addr` and `Ipv6Addr` makes for a flatter enum
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
}

impl From<IpAddr> for SocketHost<'_> {
    fn from(value: IpAddr) -> Self {
        match value {
            IpAddr::V4(addr) => Self::Ipv4(addr),
            IpAddr::V6(addr) => Self::Ipv6(addr),
        }
    }
}

impl SocketHost<'_> {
    fn try_as_ip(&self) -> Result<IpAddr, &str> {
        match *self {
            // `Url::host()` returns the host wrapped in brackets which the socket API isn't expecting
            Self::Str(s) => s.trim_matches(&['[', ']'][..]).parse().map_err(|_| s),
            Self::Ipv4(addr) => Ok(addr.into()),
            Self::Ipv6(addr) => Ok(addr.into()),
        }
    }
}
