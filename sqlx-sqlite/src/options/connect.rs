use crate::{SqliteConnectOptions, SqliteConnection};
use log::LevelFilter;
use sqlx_core::config;
use sqlx_core::connection::ConnectOptions;
use sqlx_core::error::Error;
use sqlx_core::executor::Executor;
use sqlx_core::sql_str::AssertSqlSafe;
use std::fmt::Write;
use std::str::FromStr;
use std::time::Duration;
use url::Url;

impl ConnectOptions for SqliteConnectOptions {
    type Connection = SqliteConnection;

    fn from_url(url: &Url) -> Result<Self, Error> {
        // SQLite URL parsing is handled specially;
        // we want to treat the following URLs as equivalent:
        //
        // * sqlite:foo.db
        // * sqlite://foo.db
        //
        // If we used `Url::path()`, the latter would return an empty string
        // because `foo.db` gets parsed as the hostname.
        Self::from_str(url.as_str())
    }

    fn to_url_lossy(&self) -> Url {
        self.build_url()
    }

    async fn connect(&self) -> Result<Self::Connection, Error>
    where
        Self::Connection: Sized,
    {
        let mut conn = SqliteConnection::establish(self).await?;

        let init = async {
            // Execute PRAGMAs
            conn.execute(AssertSqlSafe(self.pragma_string())).await?;

            if !self.collations.is_empty() {
                let mut locked = conn.lock_handle().await?;

                for collation in &self.collations {
                    collation.create(&mut locked.guard.handle)?;
                }
            }

            Ok::<_, Error>(())
        }
        .await;

        if let Err(error) = init {
            // Dropping the connection does not wait for its worker to release
            // the SQLite handle. Skip optional optimization, which could fail
            // before close reaches shutdown, and preserve the startup error.
            let _ = conn.shutdown().await;
            return Err(error);
        }

        Ok(conn)
    }

    fn log_statements(mut self, level: LevelFilter) -> Self {
        self.log_settings.log_statements(level);
        self
    }

    fn log_slow_statements(mut self, level: LevelFilter, duration: Duration) -> Self {
        self.log_settings.log_slow_statements(level, duration);
        self
    }

    fn __unstable_apply_driver_config(
        self,
        config: &config::drivers::Config,
    ) -> crate::Result<Self> {
        self.apply_driver_config(&config.sqlite)
    }
}

impl SqliteConnectOptions {
    /// Collect all `PRAMGA` commands into a single string
    pub(crate) fn pragma_string(&self) -> String {
        let mut string = String::new();

        for (key, opt_value) in &self.pragmas {
            if let Some(value) = opt_value {
                write!(string, "PRAGMA {key} = {value}; ").ok();
            }
        }

        string
    }
}
