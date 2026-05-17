use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;

use futures_core::future::BoxFuture;
pub(crate) use sqlx_core::migrate::*;
use sqlx_core::sql_str::AssertSqlSafe;

use crate::connection::{ConnectOptions, Connection};
use crate::error::Error;
use crate::executor::Executor;
use crate::query::query;
use crate::query_as::query_as;
use crate::query_scalar::query_scalar;
use crate::{Mssql, MssqlConnectOptions, MssqlConnection};

/// Escape a table name for safe use as an MSSQL bracket-quoted identifier (`[...]`).
fn escape_table_name(table_name: &str) -> String {
    format!("[{}]", table_name.replace(']', "]]"))
}

fn parse_for_maintenance(url: &str) -> Result<(MssqlConnectOptions, String), Error> {
    let mut options = MssqlConnectOptions::from_str(url)?;

    let database = if let Some(database) = &options.database {
        database.to_owned()
    } else {
        return Err(Error::Configuration(
            "DATABASE_URL does not specify a database".into(),
        ));
    };

    // switch us to master database for create/drop commands
    options.database = Some("master".to_owned());

    Ok((options, database))
}

impl MigrateDatabase for Mssql {
    async fn create_database(url: &str) -> Result<(), Error> {
        let (options, database) = parse_for_maintenance(url)?;
        let mut conn = options.connect().await?;

        query(
            "DECLARE @sql NVARCHAR(MAX) = N'CREATE DATABASE ' + QUOTENAME(@p1); \
             EXEC sp_executesql @sql;",
        )
        .bind(database)
        .execute(&mut conn)
        .await?;

        Ok(())
    }

    async fn database_exists(url: &str) -> Result<bool, Error> {
        let (options, database) = parse_for_maintenance(url)?;
        let mut conn = options.connect().await?;

        let exists: bool =
            query_scalar("SELECT CASE WHEN DB_ID(@p1) IS NOT NULL THEN 1 ELSE 0 END")
                .bind(database)
                .fetch_one(&mut conn)
                .await?;

        Ok(exists)
    }

    async fn drop_database(url: &str) -> Result<(), Error> {
        let (options, database) = parse_for_maintenance(url)?;
        let mut conn = options.connect().await?;

        query(
            "IF DB_ID(@p1) IS NOT NULL \
             BEGIN \
                 DECLARE @sql NVARCHAR(MAX); \
                 SET @sql = N'ALTER DATABASE ' + QUOTENAME(@p1) + N' SET SINGLE_USER WITH ROLLBACK IMMEDIATE'; \
                 EXEC sp_executesql @sql; \
                 SET @sql = N'DROP DATABASE ' + QUOTENAME(@p1); \
                 EXEC sp_executesql @sql; \
             END"
        )
        .bind(database)
        .execute(&mut conn)
        .await?;

        Ok(())
    }
}

impl Migrate for MssqlConnection {
    fn create_schema_if_not_exists<'e>(
        &'e mut self,
        schema_name: &'e str,
    ) -> BoxFuture<'e, Result<(), MigrateError>> {
        Box::pin(async move {
            query(
                "IF NOT EXISTS (SELECT * FROM sys.schemas WHERE name = @p1) \
                 BEGIN \
                     DECLARE @sql NVARCHAR(MAX) = N'CREATE SCHEMA ' + QUOTENAME(@p1); \
                     EXEC sp_executesql @sql; \
                 END",
            )
            .bind(schema_name)
            .execute(&mut *self)
            .await?;

            Ok(())
        })
    }

    fn ensure_migrations_table<'e>(
        &'e mut self,
        table_name: &'e str,
    ) -> BoxFuture<'e, Result<(), MigrateError>> {
        Box::pin(async move {
            let ident = escape_table_name(table_name);
            // Atomic check-and-create: the IF NOT EXISTS and CREATE TABLE run
            // in a single batch so concurrent migrators cannot race.
            // The WHERE clause is parameterized; the identifier must use
            // bracket-escaping because DDL identifiers can't be parameterized.
            query(AssertSqlSafe(format!(
                "IF NOT EXISTS (SELECT 1 FROM sys.tables WHERE name = @p1) \
                 CREATE TABLE {ident} ( \
                     version BIGINT PRIMARY KEY, \
                     description NVARCHAR(MAX) NOT NULL, \
                     installed_on DATETIME2 NOT NULL DEFAULT SYSUTCDATETIME(), \
                     success BIT NOT NULL, \
                     checksum VARBINARY(MAX) NOT NULL, \
                     execution_time BIGINT NOT NULL \
                 );"
            )))
            .bind(table_name)
            .execute(&mut *self)
            .await?;

            Ok(())
        })
    }

    fn dirty_version<'e>(
        &'e mut self,
        table_name: &'e str,
    ) -> BoxFuture<'e, Result<Option<i64>, MigrateError>> {
        Box::pin(async move {
            let ident = escape_table_name(table_name);
            let row: Option<(i64,)> = query_as(AssertSqlSafe(format!(
                "SELECT TOP 1 version FROM {ident} WHERE success = 0 ORDER BY version"
            )))
            .fetch_optional(self)
            .await?;

            Ok(row.map(|r| r.0))
        })
    }

    fn list_applied_migrations<'e>(
        &'e mut self,
        table_name: &'e str,
    ) -> BoxFuture<'e, Result<Vec<AppliedMigration>, MigrateError>> {
        Box::pin(async move {
            let ident = escape_table_name(table_name);
            let rows: Vec<(i64, Vec<u8>)> = query_as(AssertSqlSafe(format!(
                "SELECT version, checksum FROM {ident} ORDER BY version"
            )))
            .fetch_all(self)
            .await?;

            let migrations = rows
                .into_iter()
                .map(|(version, checksum)| AppliedMigration {
                    version,
                    checksum: checksum.into(),
                })
                .collect();

            Ok(migrations)
        })
    }

    fn lock(&mut self) -> BoxFuture<'_, Result<(), MigrateError>> {
        Box::pin(async move {
            // sp_getapplock returns a status code (0/1 = success, negative = failure)
            // but `execute` only surfaces SQL errors, not return values.
            // We use THROW to convert a failed lock acquisition into a SQL error.
            let _ = self
                .execute(
                    "DECLARE @r INT; \
                     EXEC @r = sp_getapplock @Resource = 'sqlx_migrations', @LockMode = 'Exclusive', @LockOwner = 'Session', @LockTimeout = -1; \
                     IF @r < 0 THROW 50000, 'Failed to acquire migration lock', 1;"
                )
                .await?;

            Ok(())
        })
    }

    fn unlock(&mut self) -> BoxFuture<'_, Result<(), MigrateError>> {
        Box::pin(async move {
            let _ = self
                .execute(
                    "EXEC sp_releaseapplock @Resource = 'sqlx_migrations', @LockOwner = 'Session'",
                )
                .await?;

            Ok(())
        })
    }

    fn apply<'e>(
        &'e mut self,
        table_name: &'e str,
        migration: &'e Migration,
    ) -> BoxFuture<'e, Result<Duration, MigrateError>> {
        Box::pin(async move {
            let start = Instant::now();

            if migration.no_tx {
                execute_migration(self, table_name, migration).await?;
            } else {
                // Use a single transaction for the actual migration script and the essential
                // bookkeeping so we never execute migrations twice.
                // See https://github.com/launchbadge/sqlx/issues/1966.
                let mut tx = self.begin().await?;
                execute_migration(&mut tx, table_name, migration).await?;
                tx.commit().await?;
            }

            // Update `execution_time`.
            // NOTE: The process may disconnect/die at this point, so the elapsed time value
            // might be lost. We accept this small risk since this value is not super important.
            let elapsed = start.elapsed();

            let ident = escape_table_name(table_name);

            #[allow(clippy::cast_possible_truncation)]
            let _ = query(AssertSqlSafe(format!(
                r#"
    UPDATE {ident}
    SET execution_time = @p1
    WHERE version = @p2
                "#
            )))
            .bind(elapsed.as_nanos() as i64)
            .bind(migration.version)
            .execute(self)
            .await?;

            Ok(elapsed)
        })
    }

    fn revert<'e>(
        &'e mut self,
        table_name: &'e str,
        migration: &'e Migration,
    ) -> BoxFuture<'e, Result<Duration, MigrateError>> {
        Box::pin(async move {
            let start = Instant::now();

            if migration.no_tx {
                revert_migration(self, table_name, migration).await?;
            } else {
                let mut tx = self.begin().await?;
                revert_migration(&mut tx, table_name, migration).await?;
                tx.commit().await?;
            }

            let elapsed = start.elapsed();

            Ok(elapsed)
        })
    }

    fn skip<'e>(
        &'e mut self,
        table_name: &'e str,
        migration: &'e Migration,
    ) -> BoxFuture<'e, Result<(), MigrateError>> {
        Box::pin(async move {
            let ident = escape_table_name(table_name);
            // language=TSQL
            let _ = query(AssertSqlSafe(format!(
                r#"
    INSERT INTO {ident} ( version, description, success, checksum, execution_time )
    VALUES ( @p1, @p2, 1, @p3, -1 )
                "#
            )))
            .bind(migration.version)
            .bind(&*migration.description)
            .bind(&*migration.checksum)
            .execute(self)
            .await?;

            Ok(())
        })
    }
}

async fn execute_migration(
    conn: &mut MssqlConnection,
    table_name: &str,
    migration: &Migration,
) -> Result<(), MigrateError> {
    let _ = conn
        .execute(migration.sql.clone())
        .await
        .map_err(|e| MigrateError::ExecuteMigration(e, migration.version))?;

    let ident = escape_table_name(table_name);
    let _ = query(AssertSqlSafe(format!(
        r#"
    INSERT INTO {ident} ( version, description, success, checksum, execution_time )
    VALUES ( @p1, @p2, 1, @p3, -1 )
        "#
    )))
    .bind(migration.version)
    .bind(&*migration.description)
    .bind(&*migration.checksum)
    .execute(conn)
    .await?;

    Ok(())
}

async fn revert_migration(
    conn: &mut MssqlConnection,
    table_name: &str,
    migration: &Migration,
) -> Result<(), MigrateError> {
    let _ = conn
        .execute(migration.sql.clone())
        .await
        .map_err(|e| MigrateError::ExecuteMigration(e, migration.version))?;

    let ident = escape_table_name(table_name);
    let _ = query(AssertSqlSafe(format!(
        r#"DELETE FROM {ident} WHERE version = @p1"#
    )))
    .bind(migration.version)
    .execute(conn)
    .await?;

    Ok(())
}
