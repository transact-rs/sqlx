use crate::connection::ConnectOptions;
use crate::error::Error;
use crate::executor::Executor;
use crate::{MySqlConnectOptions, MySqlConnection};
use log::LevelFilter;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_core::Url;
use std::time::Duration;

impl ConnectOptions for MySqlConnectOptions {
    type Connection = MySqlConnection;

    fn from_url(url: &Url) -> Result<Self, Error> {
        Self::parse_from_url(url)
    }

    fn to_url_lossy(&self) -> Url {
        self.build_url()
    }

    async fn connect(&self) -> Result<Self::Connection, Error>
    where
        Self::Connection: Sized,
    {
        let mut conn = MySqlConnection::establish(self).await?;

        // After the connection is established, we initialize by configuring a few
        // connection parameters

        if let Some(statement) = self.connect_init_statement() {
            conn.execute(AssertSqlSafe(statement)).await?;
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
}

impl MySqlConnectOptions {
    /// Build the `SET ...;` statement sent right after connecting.
    ///
    /// Kept as a pure function of the options (no I/O) so it can be
    /// unit-tested without a live connection.
    fn connect_init_statement(&self) -> Option<String> {
        // https://mariadb.com/kb/en/sql-mode/

        // PIPES_AS_CONCAT - Allows using the pipe character (ASCII 124) as string concatenation operator.
        //                   This means that "A" || "B" can be used in place of CONCAT("A", "B").

        // NO_ENGINE_SUBSTITUTION - If not set, if the available storage engine specified by a CREATE TABLE is
        //                          not available, a warning is given and the default storage
        //                          engine is used instead.

        // NO_ZERO_DATE - Don't allow '0000-00-00'. This is invalid in Rust.

        // NO_ZERO_IN_DATE - Don't allow 'YYYY-00-00'. This is invalid in Rust.

        // --

        // Setting the time zone allows us to assume that the output
        // from a TIMESTAMP field is UTC

        // --

        // https://mathiasbynens.be/notes/mysql-utf8mb4

        let mut sql_mode = Vec::new();
        if self.pipes_as_concat {
            sql_mode.push(r#"PIPES_AS_CONCAT"#);
        }
        if self.no_engine_substitution {
            sql_mode.push(r#"NO_ENGINE_SUBSTITUTION"#);
        }

        let mut options = Vec::new();
        if !sql_mode.is_empty() {
            options.push(format!(
                r#"sql_mode=(SELECT CONCAT(@@sql_mode, ',{}'))"#,
                sql_mode.join(",")
            ));
        }

        if let Some(timezone) = &self.timezone {
            options.push(format!("time_zone={}", quote_string_literal(timezone)));
        }

        if self.set_names {
            // As it turns out, we don't _have_ to set a collation if we don't want to.
            // We can let the server choose the default collation for the charset.
            let set_names = if let Some(collation) = &self.collation {
                format!(
                    "NAMES {} COLLATE {}",
                    quote_string_literal(&self.charset),
                    quote_string_literal(collation),
                )
            } else {
                // Leaves the default collation up to the server,
                // but ensures statements and results are encoded using the proper charset.
                format!("NAMES {}", quote_string_literal(&self.charset))
            };

            options.push(set_names);
        }

        if options.is_empty() {
            None
        } else {
            Some(format!(r#"SET {};"#, options.join(",")))
        }
    }
}

/// Render `value` as a MySQL string literal: wrapped in single quotes with
/// embedded backslashes and single quotes escaped.
///
/// `charset`, `collation` and `time_zone` are taken from the connection URL
/// (or the builder methods) and used to be interpolated into the connect-time
/// `SET` statement without quoting, so a crafted value could break out of the
/// statement and inject arbitrary SQL. Quoting them as string literals closes
/// that hole.
///
/// Doubling single quotes is safe regardless of the `NO_BACKSLASH_ESCAPES`
/// sql mode; backslashes are escaped as well so one cannot combine with a
/// following quote into an escape sequence.
fn quote_string_literal(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str(r#"\\"#),
            '\'' => quoted.push_str("''"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MySqlConnectOptions;
    use std::str::FromStr;

    fn statement_for(url: &str) -> String {
        MySqlConnectOptions::from_str(url)
            .expect("valid URL")
            .connect_init_statement()
            .expect("connect-time SET statement")
    }

    #[test]
    fn init_statement_quotes_charset_and_collation() {
        let stmt = statement_for("mysql://user:password@localhost/database");

        // Defaults (`pipes_as_concat`, `no_engine_substitution`, `+00:00`,
        // `utf8mb4`) are preserved; `NAMES` charset is quoted as a string literal.
        assert_eq!(
            stmt,
            "SET sql_mode=(SELECT CONCAT(@@sql_mode, ',PIPES_AS_CONCAT,NO_ENGINE_SUBSTITUTION')),\
             time_zone='+00:00',NAMES 'utf8mb4';"
        );
    }

    #[test]
    fn init_statement_quotes_time_zone() {
        let stmt = statement_for("mysql://user:password@localhost/database?timezone=%2B08:00");

        assert!(
            stmt.contains("time_zone='+08:00'"),
            "unexpected statement: {stmt}"
        );
    }

    #[test]
    fn init_statement_quotes_explicit_collation() {
        let stmt = statement_for("mysql://user:password@localhost/database?collation=utf8mb4_bin");

        assert!(
            stmt.contains("NAMES 'utf8mb4' COLLATE 'utf8mb4_bin'"),
            "unexpected statement: {stmt}"
        );
    }

    #[test]
    fn init_statement_neutralizes_time_zone_injection() {
        // `time_zone` used to be interpolated into `'...'` without escaping,
        // so a quote in the value could terminate the literal and start a new
        // statement (the connection enables CLIENT_MULTI_STATEMENTS).
        let stmt = statement_for(
            "mysql://user:password@localhost/database?timezone=%27%3BSELECT+SLEEP%285%29%3B--%20",
        );

        // The raw quote must not appear unescaped...
        assert!(
            !stmt.contains("time_zone='';"),
            "injection not neutralized: {stmt}"
        );
        // ...instead the quote is doubled inside the literal.
        assert!(
            stmt.contains("time_zone=''';SELECT SLEEP(5);-- '"),
            "unexpected statement: {stmt}"
        );
    }

    #[test]
    fn init_statement_neutralizes_charset_injection() {
        // `charset` used to be interpolated with no quoting at all.
        let stmt = statement_for(
            "mysql://user:password@localhost/database?charset=utf8mb4%27%3BSELECT+SLEEP%285%29%3B--%20",
        );

        assert!(
            stmt.contains("NAMES 'utf8mb4'';SELECT SLEEP(5);-- '"),
            "unexpected statement: {stmt}"
        );
    }

    #[test]
    fn init_statement_neutralizes_collation_injection() {
        let stmt = statement_for(
            "mysql://user:password@localhost/database?collation=x%27%3BSELECT+SLEEP%285%29%3B--%20",
        );

        assert!(
            stmt.contains("COLLATE 'x'';SELECT SLEEP(5);-- '"),
            "unexpected statement: {stmt}"
        );
    }

    #[test]
    fn init_statement_escapes_backslashes() {
        // A backslash must not be able to combine with the doubled quote
        // into an escape sequence that re-opens injection.
        assert_eq!(quote_string_literal("a\\b"), "'a\\\\b'");
        assert_eq!(quote_string_literal("a\\'b"), "'a\\\\''b'");
        assert_eq!(quote_string_literal("plain"), "'plain'");
        assert_eq!(quote_string_literal("it's"), "'it''s'");
    }
}
