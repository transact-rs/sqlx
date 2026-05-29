use std::borrow::Cow;
use std::env::var;
use std::fmt::{self, Display, Write};
use std::path::{Path, PathBuf};

pub use ssl_mode::PgSslMode;

use crate::{connection::LogSettings, net::tls::CertificateInput};

mod connect;
mod parse;
mod pgpass;
mod ssl_mode;

#[doc = include_str!("doc.md")]
#[derive(Debug, Clone)]
pub struct PgConnectOptions {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) socket: Option<PathBuf>,
    pub(crate) username: String,
    pub(crate) password: Option<String>,
    pub(crate) database: Option<String>,
    pub(crate) ssl_mode: PgSslMode,
    pub(crate) ssl_root_cert: Option<CertificateInput>,
    pub(crate) ssl_client_cert: Option<CertificateInput>,
    pub(crate) ssl_client_key: Option<CertificateInput>,
    pub(crate) statement_cache_capacity: usize,
    pub(crate) application_name: Option<String>,
    pub(crate) log_settings: LogSettings,
    pub(crate) extra_float_digits: Option<Cow<'static, str>>,
    pub(crate) options: Option<String>,
}

impl Default for PgConnectOptions {
    fn default() -> Self {
        Self::with_libpq_defaults()
    }
}

impl PgConnectOptions {
    /// Create a default set of connection options populated from the current environment.
    ///
    /// This behaves as if parsed from the connection string `postgres://`
    ///
    /// See the type-level documentation for details.
    ///
    /// # Deprecated
    /// This method is deprecated. Use [`with_libpq_defaults()`](Self::with_libpq_defaults) instead.
    #[deprecated(
        since = "0.9.0",
        note = "Use `with_libpq_defaults()` instead to make the behavior more explicit"
    )]
    pub fn new() -> Self {
        Self::with_libpq_defaults()
    }

    /// Create a default set of connection options populated from the current environment,
    /// mimicking libpq's default behavior.
    ///
    /// This reads environment variables (`PGHOST`, `PGPORT`, `PGUSER`, etc.) and `.pgpass` file
    /// to populate connection options, similar to how libpq behaves.
    ///
    /// This behaves as if parsed from the connection string `postgres://`
    ///
    /// See the type-level documentation for details.
    pub fn with_libpq_defaults() -> Self {
        Self::default_without_env_internal().apply_env_and_pgpass()
    }

    /// Create a default set of connection options _without_ reading from `passfile`.
    ///
    /// Equivalent to [`PgConnectOptions::with_libpq_defaults()`] but `passfile` is ignored.
    ///
    /// See the type-level documentation for details.
    ///
    /// # Deprecated
    /// This method is deprecated. Use [`default_without_env()`](Self::default_without_env) instead.
    #[deprecated(
        since = "0.9.0",
        note = "Use `default_without_env()` for explicit defaults without environment variables"
    )]
    pub fn new_without_pgpass() -> Self {
        Self::default_without_env_internal().apply_env()
    }

    /// Create connection options with sensible defaults without reading environment variables.
    ///
    /// This method provides a predictable baseline for connection options that doesn't depend
    /// on the environment. Useful for developer tools, third-party libraries, or any context
    /// where environment variables cannot be relied upon.
    ///
    /// The defaults are:
    /// - `host`: `"localhost"` (or Unix socket path if available)
    /// - `port`: `5432`
    /// - `username`: `"postgres"`
    /// - `ssl_mode`: [`PgSslMode::Prefer`]
    /// - `statement_cache_capacity`: `100`
    /// - `extra_float_digits`: `Some("2")`
    ///
    /// All other fields are set to `None`.
    ///
    /// Does not respect any `PG*` environment variables or `.pgpass` files.
    ///
    /// See the type-level documentation for details.
    pub fn default_without_env() -> Self {
        let port = 5432;
        let host = default_host(port);
        let username = "postgres".to_string();

        PgConnectOptions {
            host,
            port,
            socket: None,
            username,
            password: None,
            database: None,
            ssl_mode: PgSslMode::Prefer,
            ssl_root_cert: None,
            ssl_client_cert: None,
            ssl_client_key: None,
            statement_cache_capacity: 100,
            application_name: None,
            extra_float_digits: Some("2".into()),
            log_settings: Default::default(),
            options: None,
        }
    }

    /// Internal method that reads environment variables (libpq-style).
    ///
    /// This is used internally by `with_libpq_defaults()` and the deprecated `new_without_pgpass()`.
    fn default_without_env_internal() -> Self {
        let port = var("PGPORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5432);

        let host = var("PGHOSTADDR")
            .ok()
            .or_else(|| var("PGHOST").ok())
            .unwrap_or_else(|| default_host(port));

        let username = if let Ok(username) = var("PGUSER") {
            username
        } else if let Ok(username) = whoami::username() {
            username
        } else {
            // keep the same fallback as previous version
            "unknown".to_string()
        };

        let database = var("PGDATABASE").ok();

        let ssl_mode = var("PGSSLMODE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_default();

        PgConnectOptions {
            host,
            port,
            socket: None,
            username,
            password: var("PGPASSWORD").ok(),
            database,
            ssl_mode,
            ssl_root_cert: var("PGSSLROOTCERT").ok().map(CertificateInput::from),
            ssl_client_cert: var("PGSSLCERT").ok().map(CertificateInput::from),
            // As of writing, the implementation of `From<String>` only looks for
            // `-----BEGIN CERTIFICATE-----` and so will not attempt to parse
            // a PEM-encoded private key.
            ssl_client_key: var("PGSSLKEY").ok().map(CertificateInput::from),
            statement_cache_capacity: 100,
            application_name: var("PGAPPNAME").ok(),
            extra_float_digits: Some("2".into()),
            log_settings: Default::default(),
            options: var("PGOPTIONS").ok(),
        }
    }

    pub(crate) fn apply_pgpass(mut self) -> Self {
        if self.password.is_none() {
            self.password = pgpass::load_password(
                &self.host,
                self.port,
                &self.username,
                self.database.as_deref(),
            );
        }

        self
    }

    /// Apply environment variables only (no pgpass).
    ///
    /// This is used internally for the deprecated `new_without_pgpass()`.
    fn apply_env(self) -> Self {
        // Environment variables are already applied in default_without_env_internal()
        // This method exists for backwards compatibility but doesn't do anything
        self
    }

    /// Apply both environment variables and pgpass.
    ///
    /// This is used internally for `with_libpq_defaults()`.
    fn apply_env_and_pgpass(self) -> Self {
        // Environment variables are already applied in default_without_env_internal()
        // We just need to apply pgpass
        self.apply_pgpass()
    }

    /// Sets the name of the host to connect to.
    ///
    /// If a host name begins with a slash, it specifies
    /// Unix-domain communication rather than TCP/IP communication; the value is the name of
    /// the directory in which the socket file is stored.
    ///
    /// The default behavior when host is not specified, or is empty,
    /// is to connect to a Unix-domain socket
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .host("localhost");
    /// ```
    pub fn host(mut self, host: &str) -> Self {
        host.clone_into(&mut self.host);
        self
    }

    /// Sets the port to connect to at the server host.
    ///
    /// The default port for PostgreSQL is `5432`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .port(5432);
    /// ```
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Sets a custom path to a directory containing a unix domain socket,
    /// switching the connection method from TCP to the corresponding socket.
    ///
    /// By default set to `None`.
    pub fn socket(mut self, path: impl AsRef<Path>) -> Self {
        self.socket = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the username to connect as.
    ///
    /// Defaults to be the same as the operating system name of
    /// the user running the application.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .username("postgres");
    /// ```
    pub fn username(mut self, username: &str) -> Self {
        username.clone_into(&mut self.username);
        self
    }

    /// Sets the password to use if the server demands password authentication.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .username("root")
    ///     .password("safe-and-secure");
    /// ```
    pub fn password(mut self, password: &str) -> Self {
        self.password = Some(password.to_owned());
        self
    }

    /// Sets the database name. Defaults to be the same as the user name.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .database("postgres");
    /// ```
    pub fn database(mut self, database: &str) -> Self {
        self.database = Some(database.to_owned());
        self
    }

    /// Sets whether or with what priority a secure SSL TCP/IP connection will be negotiated
    /// with the server.
    ///
    /// By default, the SSL mode is [`Prefer`](PgSslMode::Prefer), and the client will
    /// first attempt an SSL connection but fallback to a non-SSL connection on failure.
    ///
    /// Ignored for Unix domain socket communication.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::{PgSslMode, PgConnectOptions};
    /// let options = PgConnectOptions::new()
    ///     .ssl_mode(PgSslMode::Require);
    /// ```
    pub fn ssl_mode(mut self, mode: PgSslMode) -> Self {
        self.ssl_mode = mode;
        self
    }

    /// Sets the name of a file containing SSL certificate authority (CA) certificate(s).
    /// If the file exists, the server's certificate will be verified to be signed by
    /// one of these authorities.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::{PgSslMode, PgConnectOptions};
    /// let options = PgConnectOptions::new()
    ///     // Providing a CA certificate with less than VerifyCa is pointless
    ///     .ssl_mode(PgSslMode::VerifyCa)
    ///     .ssl_root_cert("./ca-certificate.crt");
    /// ```
    pub fn ssl_root_cert(mut self, cert: impl AsRef<Path>) -> Self {
        self.ssl_root_cert = Some(CertificateInput::File(cert.as_ref().to_path_buf()));
        self
    }

    /// Sets the name of a file containing SSL client certificate.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::{PgSslMode, PgConnectOptions};
    /// let options = PgConnectOptions::new()
    ///     // Providing a CA certificate with less than VerifyCa is pointless
    ///     .ssl_mode(PgSslMode::VerifyCa)
    ///     .ssl_client_cert("./client.crt");
    /// ```
    pub fn ssl_client_cert(mut self, cert: impl AsRef<Path>) -> Self {
        self.ssl_client_cert = Some(CertificateInput::File(cert.as_ref().to_path_buf()));
        self
    }

    /// Sets the SSL client certificate as a PEM-encoded byte slice.
    ///
    /// This should be an ASCII-encoded blob that starts with `-----BEGIN CERTIFICATE-----`.
    ///
    /// # Example
    /// Note: embedding SSL certificates and keys in the binary is not advised.
    /// This is for illustration purposes only.
    ///
    /// ```rust
    /// # use sqlx_postgres::{PgSslMode, PgConnectOptions};
    ///
    /// const CERT: &[u8] = b"\
    /// -----BEGIN CERTIFICATE-----
    /// <Certificate data here.>
    /// -----END CERTIFICATE-----";
    ///    
    /// let options = PgConnectOptions::new()
    ///     // Providing a CA certificate with less than VerifyCa is pointless
    ///     .ssl_mode(PgSslMode::VerifyCa)
    ///     .ssl_client_cert_from_pem(CERT);
    /// ```
    pub fn ssl_client_cert_from_pem(mut self, cert: impl AsRef<[u8]>) -> Self {
        self.ssl_client_cert = Some(CertificateInput::Inline(cert.as_ref().to_vec()));
        self
    }

    /// Sets the name of a file containing SSL client key.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::{PgSslMode, PgConnectOptions};
    /// let options = PgConnectOptions::new()
    ///     // Providing a CA certificate with less than VerifyCa is pointless
    ///     .ssl_mode(PgSslMode::VerifyCa)
    ///     .ssl_client_key("./client.key");
    /// ```
    pub fn ssl_client_key(mut self, key: impl AsRef<Path>) -> Self {
        self.ssl_client_key = Some(CertificateInput::File(key.as_ref().to_path_buf()));
        self
    }

    /// Sets the SSL client key as a PEM-encoded byte slice.
    ///
    /// This should be an ASCII-encoded blob that starts with `-----BEGIN PRIVATE KEY-----`.
    ///
    /// # Example
    /// Note: embedding SSL certificates and keys in the binary is not advised.
    /// This is for illustration purposes only.
    ///
    /// ```rust
    /// # use sqlx_postgres::{PgSslMode, PgConnectOptions};
    ///
    /// const KEY: &[u8] = b"\
    /// -----BEGIN PRIVATE KEY-----
    /// <Private key data here.>
    /// -----END PRIVATE KEY-----";
    ///
    /// let options = PgConnectOptions::new()
    ///     // Providing a CA certificate with less than VerifyCa is pointless
    ///     .ssl_mode(PgSslMode::VerifyCa)
    ///     .ssl_client_key_from_pem(KEY);
    /// ```
    pub fn ssl_client_key_from_pem(mut self, key: impl AsRef<[u8]>) -> Self {
        self.ssl_client_key = Some(CertificateInput::Inline(key.as_ref().to_vec()));
        self
    }

    /// Sets PEM encoded trusted SSL Certificate Authorities (CA).
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::{PgSslMode, PgConnectOptions};
    /// let options = PgConnectOptions::new()
    ///     // Providing a CA certificate with less than VerifyCa is pointless
    ///     .ssl_mode(PgSslMode::VerifyCa)
    ///     .ssl_root_cert_from_pem(vec![]);
    /// ```
    pub fn ssl_root_cert_from_pem(mut self, pem_certificate: Vec<u8>) -> Self {
        self.ssl_root_cert = Some(CertificateInput::Inline(pem_certificate));
        self
    }

    /// Sets the capacity of the connection's statement cache in a number of stored
    /// distinct statements. Caching is handled using LRU, meaning when the
    /// amount of queries hits the defined limit, the oldest statement will get
    /// dropped.
    ///
    /// The default cache capacity is 100 statements.
    pub fn statement_cache_capacity(mut self, capacity: usize) -> Self {
        self.statement_cache_capacity = capacity;
        self
    }

    /// Sets the application name. Defaults to None
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .application_name("my-app");
    /// ```
    pub fn application_name(mut self, application_name: &str) -> Self {
        self.application_name = Some(application_name.to_owned());
        self
    }

    /// Sets or removes the `extra_float_digits` connection option.
    ///
    /// This changes the default precision of floating-point values returned in text mode (when
    /// not using prepared statements such as calling methods of [`Executor`] directly).
    ///
    /// Historically, Postgres would by default round floating-point values to 6 and 15 digits
    /// for `float4`/`REAL` (`f32`) and `float8`/`DOUBLE` (`f64`), respectively, which would mean
    /// that the returned value may not be exactly the same as its representation in Postgres.
    ///
    /// The nominal range for this value is `-15` to `3`, where negative values for this option
    /// cause floating-points to be rounded to that many fewer digits than normal (`-1` causes
    /// `float4` to be rounded to 5 digits instead of six, or 14 instead of 15 for `float8`),
    /// positive values cause Postgres to emit that many extra digits of precision over default
    /// (or simply use maximum precision in Postgres 12 and later),
    /// and 0 means keep the default behavior (or the "old" behavior described above
    /// as of Postgres 12).
    ///
    /// SQLx sets this value to 3 by default, which tells Postgres to return floating-point values
    /// at their maximum precision in the hope that the parsed value will be identical to its
    /// counterpart in Postgres. This is also the default in Postgres 12 and later anyway.
    ///
    /// However, older versions of Postgres and alternative implementations that talk the Postgres
    /// protocol may not support this option, or the full range of values.
    ///
    /// If you get an error like "unknown option `extra_float_digits`" when connecting, try
    /// setting this to `None` or consult the manual of your database for the allowed range
    /// of values.
    ///
    /// For more information, see:
    /// * [Postgres manual, 20.11.2: Client Connection Defaults; Locale and Formatting][20.11.2]
    /// * [Postgres manual, 8.1.3: Numeric Types; Floating-point Types][8.1.3]
    ///
    /// [`Executor`]: crate::executor::Executor
    /// [20.11.2]: https://www.postgresql.org/docs/current/runtime-config-client.html#RUNTIME-CONFIG-CLIENT-FORMAT
    /// [8.1.3]: https://www.postgresql.org/docs/current/datatype-numeric.html#DATATYPE-FLOAT
    ///
    /// ### Examples
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    ///
    /// let mut options = PgConnectOptions::new()
    ///     // for Redshift and Postgres 10
    ///     .extra_float_digits(2);
    ///
    /// let mut options = PgConnectOptions::new()
    ///     // don't send the option at all (Postgres 9 and older)
    ///     .extra_float_digits(None);
    /// ```
    pub fn extra_float_digits(mut self, extra_float_digits: impl Into<Option<i8>>) -> Self {
        self.extra_float_digits = extra_float_digits.into().map(|it| it.to_string().into());
        self
    }

    /// Set additional startup options for the connection as a list of key-value pairs.
    ///
    /// Escapes the options’ backslash and space characters as per
    /// https://www.postgresql.org/docs/current/libpq-connect.html#LIBPQ-CONNECT-OPTIONS
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .options([("geqo", "off"), ("statement_timeout", "5min")]);
    /// ```
    pub fn options<K, V, I>(mut self, options: I) -> Self
    where
        K: Display,
        V: Display,
        I: IntoIterator<Item = (K, V)>,
    {
        // Do this in here so `options_str` is only set if we have an option to insert
        let options_str = self.options.get_or_insert_with(String::new);
        for (k, v) in options {
            if !options_str.is_empty() {
                options_str.push(' ');
            }

            options_str.push_str("-c ");
            write!(PgOptionsWriteEscaped(options_str), "{k}={v}").ok();
        }
        self
    }

    /// We try using a socket if hostname starts with `/` or if socket parameter
    /// is specified.
    pub(crate) fn fetch_socket(&self) -> Option<String> {
        match self.socket {
            Some(ref socket) => {
                let full_path = format!("{}/.s.PGSQL.{}", socket.display(), self.port);
                Some(full_path)
            }
            None if self.host.starts_with('/') => {
                let full_path = format!("{}/.s.PGSQL.{}", self.host, self.port);
                Some(full_path)
            }
            _ => None,
        }
    }
}

impl PgConnectOptions {
    /// Get the current host.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .host("127.0.0.1");
    /// assert_eq!(options.get_host(), "127.0.0.1");
    /// ```
    pub fn get_host(&self) -> &str {
        &self.host
    }

    /// Get the server's port.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .port(6543);
    /// assert_eq!(options.get_port(), 6543);
    /// ```
    pub fn get_port(&self) -> u16 {
        self.port
    }

    /// Get the socket path.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .socket("/tmp");
    /// assert!(options.get_socket().is_some());
    /// ```
    pub fn get_socket(&self) -> Option<&PathBuf> {
        self.socket.as_ref()
    }

    /// Get the server's port.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .username("foo");
    /// assert_eq!(options.get_username(), "foo");
    /// ```
    pub fn get_username(&self) -> &str {
        &self.username
    }

    /// Get the current database name.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .database("postgres");
    /// assert!(options.get_database().is_some());
    /// ```
    pub fn get_database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// Get the SSL mode.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::{PgConnectOptions, PgSslMode};
    /// let options = PgConnectOptions::new();
    /// assert!(matches!(options.get_ssl_mode(), PgSslMode::Prefer));
    /// ```
    pub fn get_ssl_mode(&self) -> PgSslMode {
        self.ssl_mode
    }

    /// Get the application name.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .application_name("service");
    /// assert!(options.get_application_name().is_some());
    /// ```
    pub fn get_application_name(&self) -> Option<&str> {
        self.application_name.as_deref()
    }

    /// Get the options.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use sqlx_postgres::PgConnectOptions;
    /// let options = PgConnectOptions::new()
    ///     .options([("foo", "bar")]);
    /// assert!(options.get_options().is_some());
    /// ```
    pub fn get_options(&self) -> Option<&str> {
        self.options.as_deref()
    }
}

fn default_host(port: u16) -> String {
    // try to check for the existence of a unix socket and uses that
    let socket = format!(".s.PGSQL.{port}");
    let candidates = [
        "/var/run/postgresql", // Debian
        "/private/tmp",        // OSX (homebrew)
        "/tmp",                // Default
    ];

    for candidate in &candidates {
        if Path::new(candidate).join(&socket).exists() {
            return candidate.to_string();
        }
    }

    // fallback to localhost if no socket was found
    "localhost".to_owned()
}

/// Writer that escapes passed-in PostgreSQL options.
///
/// Escapes backslashes and spaces with an additional backslash according to
/// https://www.postgresql.org/docs/current/libpq-connect.html#LIBPQ-CONNECT-OPTIONS
#[derive(Debug)]
struct PgOptionsWriteEscaped<'a>(&'a mut String);

impl Write for PgOptionsWriteEscaped<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut span_start = 0;

        for (span_end, matched) in s.match_indices([' ', '\\']) {
            write!(self.0, r"{}\{matched}", &s[span_start..span_end])?;
            span_start = span_end + matched.len();
        }

        // Write the rest of the string after the last match, or all of it if no matches
        self.0.push_str(&s[span_start..]);

        Ok(())
    }

    fn write_char(&mut self, ch: char) -> fmt::Result {
        if matches!(ch, ' ' | '\\') {
            self.0.push('\\');
        }

        self.0.push(ch);

        Ok(())
    }
}

#[test]
fn test_options_formatting() {
    let options = PgConnectOptions::default_without_env().options([("geqo", "off")]);
    assert_eq!(options.options, Some("-c geqo=off".to_string()));
    let options = options.options([("search_path", "sqlx")]);
    assert_eq!(
        options.options,
        Some("-c geqo=off -c search_path=sqlx".to_string())
    );
    let options = PgConnectOptions::default_without_env()
        .options([("geqo", "off"), ("statement_timeout", "5min")]);
    assert_eq!(
        options.options,
        Some("-c geqo=off -c statement_timeout=5min".to_string())
    );
    // https://www.postgresql.org/docs/current/libpq-connect.html#LIBPQ-CONNECT-OPTIONS
    let options = PgConnectOptions::default_without_env()
        .options([("application_name", r"/back\slash/ and\ spaces")]);
    assert_eq!(
        options.options,
        Some(r"-c application_name=/back\\slash/\ and\\\ spaces".to_string())
    );
    let options = PgConnectOptions::default_without_env();
    assert_eq!(options.options, None);
}

#[test]
fn test_pg_write_escaped() {
    let mut buf = String::new();
    let mut x = PgOptionsWriteEscaped(&mut buf);
    x.write_str("x").unwrap();
    x.write_str("").unwrap();
    x.write_char('\\').unwrap();
    x.write_str("y \\").unwrap();
    x.write_char(' ').unwrap();
    x.write_char('z').unwrap();
    assert_eq!(buf, r"x\\y\ \\\ z");
}

#[test]
#[allow(deprecated)]
fn test_deprecated_api_backwards_compatibility() {
    // Test that deprecated methods still work correctly for backwards compatibility

    // Test deprecated new() method
    let options = PgConnectOptions::new();
    assert_eq!(options.port, 5432);
    assert_eq!(options.statement_cache_capacity, 100);
    assert!(options.extra_float_digits.is_some());

    // Test deprecated new_without_pgpass() method
    let options = PgConnectOptions::new_without_pgpass();
    assert_eq!(options.port, 5432);
    assert_eq!(options.statement_cache_capacity, 100);

    // Verify the deprecated methods can be chained with builder methods
    let options = PgConnectOptions::new()
        .host("example.com")
        .port(5433)
        .username("testuser")
        .database("testdb");

    assert_eq!(options.get_host(), "example.com");
    assert_eq!(options.get_port(), 5433);
    assert_eq!(options.get_username(), "testuser");
    assert_eq!(options.get_database(), Some("testdb"));

    // Verify new_without_pgpass() works with builder pattern
    let options = PgConnectOptions::new_without_pgpass()
        .host("localhost")
        .username("postgres");

    assert_eq!(options.get_host(), "localhost");
    assert_eq!(options.get_username(), "postgres");
}

#[test]
fn test_new_api_without_environment() {
    // Test the new API methods that don't read environment variables

    // Test default_without_env() provides hardcoded defaults
    let options = PgConnectOptions::default_without_env();
    assert_eq!(options.port, 5432);
    assert_eq!(options.username, "postgres"); // Hardcoded, not OS username
    assert_eq!(options.ssl_mode, PgSslMode::Prefer);
    assert_eq!(options.statement_cache_capacity, 100);
    assert!(options.extra_float_digits.is_some());
    assert!(options.password.is_none());
    assert!(options.database.is_none());

    // Test builder pattern with default_without_env()
    let options = PgConnectOptions::default_without_env()
        .host("example.com")
        .port(5433)
        .username("myuser")
        .database("mydb")
        .password("mypass");

    assert_eq!(options.get_host(), "example.com");
    assert_eq!(options.get_port(), 5433);
    assert_eq!(options.get_username(), "myuser");
    assert_eq!(options.get_database(), Some("mydb"));

    // Test with_libpq_defaults()
    let options = PgConnectOptions::with_libpq_defaults();
    assert_eq!(options.port, 5432);
    assert_eq!(options.statement_cache_capacity, 100);
}
