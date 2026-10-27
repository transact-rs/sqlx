use crate::error::Error;
use crate::{PgConnectOptions, PgSslMode};
use sqlx_core::net::TcpKeepalive;
use sqlx_core::percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use sqlx_core::Url;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

impl PgConnectOptions {
    pub(crate) fn parse_from_url(url: &Url) -> Result<Self, Error> {
        let mut options = Self::new_without_pgpass();

        if let Some(host) = url.host_str() {
            let host_decoded = percent_decode_str(host);
            options = match host_decoded.clone().next() {
                Some(b'/') => options.socket(&*host_decoded.decode_utf8().map_err(Error::config)?),
                _ => options.host(host),
            }
        }

        if let Some(port) = url.port() {
            options = options.port(port);
        }

        let username = url.username();
        if !username.is_empty() {
            options = options.username(
                &percent_decode_str(username)
                    .decode_utf8()
                    .map_err(Error::config)?,
            );
        }

        if let Some(password) = url.password() {
            options = options.password(
                &percent_decode_str(password)
                    .decode_utf8()
                    .map_err(Error::config)?,
            );
        }

        let path = url.path().trim_start_matches('/');
        if !path.is_empty() {
            options = options.database(
                &percent_decode_str(path)
                    .decode_utf8()
                    .map_err(Error::config)?,
            );
        }

        // Resolved after the loop, so the parameters can come in any order.
        let mut keepalives = None;
        let mut keepalive = TcpKeepalive::new();
        let mut keepalive_parameters = false;

        for (key, value) in url.query_pairs().into_iter() {
            match &*key {
                "sslmode" | "ssl-mode" => {
                    options = options.ssl_mode(value.parse().map_err(Error::config)?);
                }

                "sslrootcert" | "ssl-root-cert" | "ssl-ca" => {
                    options = options.ssl_root_cert(&*value);
                }

                "sslcert" | "ssl-cert" => options = options.ssl_client_cert(&*value),

                "sslkey" | "ssl-key" => options = options.ssl_client_key(&*value),

                "statement-cache-capacity" => {
                    options =
                        options.statement_cache_capacity(value.parse().map_err(Error::config)?);
                }

                "host" => {
                    if value.starts_with('/') {
                        options = options.socket(&*value);
                    } else {
                        options = options.host(&value);
                    }
                }

                "hostaddr" => {
                    value.parse::<IpAddr>().map_err(Error::config)?;
                    options = options.host(&value)
                }

                "port" => options = options.port(value.parse().map_err(Error::config)?),

                "dbname" => options = options.database(&value),

                "user" => options = options.username(&value),

                "password" => options = options.password(&value),

                "application_name" => options = options.application_name(&value),

                // Any nonzero integer turns keepalive on, as in libpq.
                "keepalives" => {
                    let enabled: i32 = value.parse().map_err(Error::config)?;
                    keepalives = Some(enabled != 0);
                }

                // Seconds and a probe count, as in libpq, where `0` keeps the system default.
                "keepalives_idle" => {
                    keepalive = keepalive
                        .with_idle(Duration::from_secs(value.parse().map_err(Error::config)?));
                    keepalive_parameters = true;
                }

                "keepalives_interval" => {
                    keepalive = keepalive
                        .with_interval(Duration::from_secs(value.parse().map_err(Error::config)?));
                    keepalive_parameters = true;
                }

                "keepalives_count" => {
                    keepalive = keepalive.with_retries(value.parse().map_err(Error::config)?);
                    keepalive_parameters = true;
                }

                // Milliseconds, as in libpq, where `0` keeps the system default.
                "tcp_user_timeout" => {
                    let millis: u64 = value.parse().map_err(Error::config)?;
                    options = options
                        .tcp_user_timeout((millis > 0).then_some(Duration::from_millis(millis)));
                }

                "options" => {
                    if let Some(options) = options.options.as_mut() {
                        options.push(' ');
                        options.push_str(&value);
                    } else {
                        options.options = Some(value.to_string());
                    }
                }

                k if k.starts_with("options[") => {
                    if let Some(key) = k.strip_prefix("options[").unwrap().strip_suffix(']') {
                        options = options.options([(key, &*value)]);
                    }
                }

                _ => tracing::warn!(%key, %value, "ignoring unrecognized connect parameter"),
            }
        }

        // Unlike libpq, keepalive is off unless the URL asks for it.
        if keepalives.unwrap_or(keepalive_parameters) {
            options = options.tcp_keepalive(keepalive);
        }

        let options = options.apply_pgpass();

        Ok(options)
    }

    pub(crate) fn build_url(&self) -> Url {
        let host = match &self.socket {
            Some(socket) => {
                utf8_percent_encode(&socket.to_string_lossy(), NON_ALPHANUMERIC).to_string()
            }
            None => self.host.to_owned(),
        };

        let mut url = Url::parse(&format!(
            "postgres://{}@{}:{}",
            self.username, host, self.port
        ))
        .expect("BUG: generated un-parseable URL");

        if let Some(password) = &self.password {
            let password = utf8_percent_encode(password, NON_ALPHANUMERIC).to_string();
            let _ = url.set_password(Some(&password));
        }

        if let Some(database) = &self.database {
            url.set_path(database);
        }

        let ssl_mode = match self.ssl_options.ssl_mode {
            PgSslMode::Allow => "allow",
            PgSslMode::Disable => "disable",
            PgSslMode::Prefer => "prefer",
            PgSslMode::Require => "require",
            PgSslMode::VerifyCa => "verify-ca",
            PgSslMode::VerifyFull => "verify-full",
        };
        url.query_pairs_mut().append_pair("sslmode", ssl_mode);

        if let Some(ssl_root_cert) = &self.ssl_options.ssl_root_cert {
            url.query_pairs_mut()
                .append_pair("sslrootcert", &ssl_root_cert.to_string());
        }

        if let Some(ssl_client_cert) = &self.ssl_options.ssl_client_cert {
            url.query_pairs_mut()
                .append_pair("sslcert", &ssl_client_cert.to_string());
        }

        if let Some(ssl_client_key) = &self.ssl_options.ssl_client_key {
            url.query_pairs_mut()
                .append_pair("sslkey", &ssl_client_key.to_string());
        }

        if let Some(keepalive) = &self.tcp_keepalive {
            let mut query = url.query_pairs_mut();
            query.append_pair("keepalives", "1");
            if let Some(idle) = keepalive.idle.filter(|idle| !idle.is_zero()) {
                query.append_pair("keepalives_idle", &whole_seconds(idle).to_string());
            }
            if let Some(interval) = keepalive.interval.filter(|interval| !interval.is_zero()) {
                query.append_pair("keepalives_interval", &whole_seconds(interval).to_string());
            }
            if let Some(retries) = keepalive.retries {
                query.append_pair("keepalives_count", &retries.to_string());
            }
        }

        if let Some(timeout) = self.tcp_user_timeout.filter(|timeout| !timeout.is_zero()) {
            // Whole milliseconds, rounding a shorter timeout up as the socket option does.
            let millis = std::cmp::max(timeout.as_millis(), 1);
            url.query_pairs_mut()
                .append_pair("tcp_user_timeout", &millis.to_string());
        }

        url.query_pairs_mut().append_pair(
            "statement-cache-capacity",
            &self.statement_cache_capacity.to_string(),
        );

        url
    }
}

impl FromStr for PgConnectOptions {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let url: Url = s.parse().map_err(Error::config)?;

        Self::parse_from_url(&url)
    }
}

/// Rounds up to whole seconds, as the keepalive socket options do, so a URL built from the options
/// connects with the same values. Truncating to milliseconds first would write a sub-millisecond
/// value as `0`, the system default.
fn whole_seconds(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0))
}

#[test]
fn it_parses_socket_correctly_from_parameter() {
    let url = "postgres:///?host=/var/run/postgres/";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(Some("/var/run/postgres/".into()), opts.socket);
}

#[test]
fn it_parses_host_correctly_from_parameter() {
    let url = "postgres:///?host=google.database.com";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(None, opts.socket);
    assert_eq!("google.database.com", &opts.host);
}

#[test]
fn it_parses_hostaddr_correctly_from_parameter() {
    let url = "postgres:///?hostaddr=8.8.8.8";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(None, opts.socket);
    assert_eq!("8.8.8.8", &opts.host);
}

#[test]
fn it_parses_port_correctly_from_parameter() {
    let url = "postgres:///?port=1234";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(None, opts.socket);
    assert_eq!(1234, opts.port);
}

#[test]
fn it_parses_dbname_correctly_from_parameter() {
    let url = "postgres:///?dbname=some_db";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(None, opts.socket);
    assert_eq!(Some("some_db"), opts.database.as_deref());
}

#[test]
fn it_parses_user_correctly_from_parameter() {
    let url = "postgres:///?user=some_user";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(None, opts.socket);
    assert_eq!("some_user", opts.username);
}

#[test]
fn it_parses_password_correctly_from_parameter() {
    let url = "postgres:///?password=some_pass";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(None, opts.socket);
    assert_eq!(Some("some_pass"), opts.password.as_deref());
}

#[test]
fn it_parses_application_name_correctly_from_parameter() {
    let url = "postgres:///?application_name=some_name";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(Some("some_name"), opts.application_name.as_deref());
}

#[test]
fn it_parses_username_with_at_sign_correctly() {
    let url = "postgres://user@hostname:password@hostname:5432/database";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!("user@hostname", &opts.username);
}

#[test]
fn it_parses_password_with_non_ascii_chars_correctly() {
    let url = "postgres://username:p@ssw0rd@hostname:5432/database";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(Some("p@ssw0rd".into()), opts.password);
}

#[test]
fn it_parses_socket_correctly_percent_encoded() {
    let url = "postgres://%2Fvar%2Flib%2Fpostgres/database";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(Some("/var/lib/postgres/".into()), opts.socket);
}
#[test]
fn it_parses_socket_correctly_with_username_percent_encoded() {
    let url = "postgres://some_user@%2Fvar%2Flib%2Fpostgres/database";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!("some_user", opts.username);
    assert_eq!(Some("/var/lib/postgres/".into()), opts.socket);
    assert_eq!(Some("database"), opts.database.as_deref());
}
#[test]
fn it_parses_libpq_options_correctly() {
    let url = "postgres:///?options=-c%20synchronous_commit%3Doff%20--search_path%3Dpostgres";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(
        Some("-c synchronous_commit=off --search_path=postgres".into()),
        opts.options
    );
}
#[test]
fn it_parses_sqlx_options_correctly() {
    let url = "postgres:///?options[synchronous_commit]=off&options[search_path]=postgres";
    let opts = PgConnectOptions::from_str(url).unwrap();

    assert_eq!(
        Some("-c synchronous_commit=off -c search_path=postgres".into()),
        opts.options
    );
}

#[test]
fn it_returns_the_parsed_url_when_socket() {
    let url = "postgres://username@%2Fvar%2Flib%2Fpostgres/database";
    let opts = PgConnectOptions::from_str(url).unwrap();

    let mut expected_url = Url::parse(url).unwrap();
    // PgConnectOptions defaults
    let query_string = "sslmode=prefer&statement-cache-capacity=100";
    let port = 5432;
    expected_url.set_query(Some(query_string));
    let _ = expected_url.set_port(Some(port));

    assert_eq!(expected_url, opts.build_url());
}

#[test]
fn it_returns_the_parsed_url_when_host() {
    let url = "postgres://username:p@ssw0rd@hostname:5432/database";
    let opts = PgConnectOptions::from_str(url).unwrap();

    let mut expected_url = Url::parse(url).unwrap();
    // PgConnectOptions defaults
    let query_string = "sslmode=prefer&statement-cache-capacity=100";
    expected_url.set_query(Some(query_string));

    assert_eq!(expected_url, opts.build_url());
}

#[test]
fn built_url_can_be_parsed() {
    let url = "postgres://username:p@ssw0rd@hostname:5432/database";
    let opts = PgConnectOptions::from_str(url).unwrap();

    let parsed = PgConnectOptions::from_str(opts.build_url().as_ref());

    assert!(parsed.is_ok());
}

#[test]
fn it_parses_tcp_user_timeout_in_milliseconds() {
    let opts = PgConnectOptions::from_str("postgres:///?tcp_user_timeout=60000").unwrap();
    assert_eq!(Some(Duration::from_secs(60)), opts.tcp_user_timeout);

    let url = opts.build_url();
    assert!(url
        .query_pairs()
        .any(|(key, value)| key == "tcp_user_timeout" && value == "60000"));

    // As in libpq, zero keeps the system default.
    let opts = PgConnectOptions::from_str("postgres:///?tcp_user_timeout=0").unwrap();
    assert_eq!(None, opts.tcp_user_timeout);
    assert!(!opts
        .build_url()
        .query_pairs()
        .any(|(key, _)| key == "tcp_user_timeout"));

    assert!(PgConnectOptions::from_str("postgres:///?tcp_user_timeout=soon").is_err());
}

#[test]
fn it_parses_keepalive_parameters_as_in_libpq() {
    let opts = PgConnectOptions::from_str(
        "postgres:///?keepalives_idle=30&keepalives_interval=10&keepalives_count=3",
    )
    .unwrap();
    let keepalive = TcpKeepalive::new()
        .with_idle(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    assert_eq!(Some(keepalive), opts.tcp_keepalive);

    let reparsed = PgConnectOptions::parse_from_url(&opts.build_url()).unwrap();
    assert_eq!(Some(keepalive), reparsed.tcp_keepalive);

    // The fields are public, so a sub-second value can bypass the builders' rounding. The built
    // URL must round it up to the second the socket gets, not down to the system default.
    let mut sub_second = TcpKeepalive::new();
    sub_second.idle = Some(Duration::from_micros(500));
    sub_second.interval = Some(Duration::from_millis(1500));
    let reparsed =
        PgConnectOptions::parse_from_url(&opts.tcp_keepalive(sub_second).build_url()).unwrap();
    assert_eq!(
        Some(
            TcpKeepalive::new()
                .with_idle(Duration::from_secs(1))
                .with_interval(Duration::from_secs(2))
        ),
        reparsed.tcp_keepalive
    );

    // Any nonzero `keepalives` turns keepalive on, as in libpq.
    let opts = PgConnectOptions::from_str("postgres:///?keepalives=-1").unwrap();
    assert_eq!(Some(TcpKeepalive::new()), opts.tcp_keepalive);

    // `keepalives=1` alone enables keepalive with the system defaults.
    let opts = PgConnectOptions::from_str("postgres:///?keepalives=1").unwrap();
    assert_eq!(Some(TcpKeepalive::new()), opts.tcp_keepalive);

    // Unlike libpq, keepalive is off by default.
    let opts = PgConnectOptions::from_str("postgres:///").unwrap();
    assert_eq!(None, opts.tcp_keepalive);
    assert!(!opts
        .build_url()
        .query_pairs()
        .any(|(key, _)| key.starts_with("keepalives")));

    // `keepalives=0` wins over the parameters, wherever it appears.
    let opts = PgConnectOptions::from_str("postgres:///?keepalives_idle=30&keepalives=0").unwrap();
    assert_eq!(None, opts.tcp_keepalive);

    assert!(PgConnectOptions::from_str("postgres:///?keepalives_idle=soon").is_err());
    assert!(PgConnectOptions::from_str("postgres:///?keepalives=yes").is_err());
}
