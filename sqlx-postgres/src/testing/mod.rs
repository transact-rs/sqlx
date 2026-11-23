use std::future::Future;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE;
use base64::Engine as _;
use sha2::{Digest, Sha512};
use sqlx_core::connection::{ConnectOptions, Connection};
use sqlx_core::migrate::Migrator;
use sqlx_core::query_builder::QueryBuilder;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::sql_str::AssertSqlSafe;

use crate::error::Error;
use crate::executor::Executor;
use crate::pool::{Pool, PoolOptions};
use crate::query::query;
use crate::{PgConnectOptions, PgConnection, Postgres};

pub(crate) use sqlx_core::testing::*;

// Using a blocking `OnceLock` here because the critical sections are short.
static MASTER_POOL: OnceLock<Pool<Postgres>> = OnceLock::new();
// Automatically delete any databases created before the start of the test binary.

// Templates not used for this long are dropped when a new one is built.
const TEMPLATE_MAX_AGE_DAYS: i32 = 7;
// How long to wait for the migrating connection to disappear from `pg_stat_activity`.
const TEMPLATE_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

impl TestSupport for Postgres {
    fn test_context(
        args: &TestArgs,
    ) -> impl Future<Output = Result<TestContext<Self>, Error>> + Send + '_ {
        test_context(args)
    }

    async fn cleanup_test(db_name: &str) -> Result<(), Error> {
        let mut conn = MASTER_POOL
            .get()
            .expect("cleanup_test() invoked outside `#[sqlx::test]`")
            .acquire()
            .await?;

        do_cleanup(&mut conn, db_name).await
    }

    async fn cleanup_test_dbs() -> Result<Option<usize>, Error> {
        let url = dotenvy::var("DATABASE_URL").expect("DATABASE_URL must be set");

        let mut conn = PgConnection::connect(&url).await?;

        let delete_db_names: Vec<String> = query_scalar("select db_name from _sqlx_test.databases")
            .fetch_all(&mut conn)
            .await?;

        if delete_db_names.is_empty() {
            return Ok(None);
        }

        let mut deleted_db_names = Vec::with_capacity(delete_db_names.len());

        let mut builder = QueryBuilder::new("drop database if exists ");

        for db_name in &delete_db_names {
            builder.push(db_name);

            match builder.build().execute(&mut conn).await {
                Ok(_deleted) => {
                    deleted_db_names.push(db_name);
                }
                // Assume a database error just means the DB is still in use.
                Err(Error::Database(dbe)) => {
                    eprintln!("could not clean test database {db_name:?}: {dbe}")
                }
                // Bubble up other errors
                Err(e) => return Err(e),
            }

            builder.reset();
        }

        query("delete from _sqlx_test.databases where db_name = any($1::text[])")
            .bind(&deleted_db_names)
            .execute(&mut conn)
            .await?;

        let _ = conn.close().await;
        Ok(Some(delete_db_names.len()))
    }

    async fn snapshot(_conn: &mut Self::Connection) -> Result<FixtureSnapshot<Self>, Error> {
        // TODO: I want to get the testing feature out the door so this will have to wait,
        // but I'm keeping the code around for now because I plan to come back to it.
        todo!()
    }
}

async fn test_context(args: &TestArgs) -> Result<TestContext<Postgres>, Error> {
    let url = dotenvy::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let master_opts = PgConnectOptions::from_str(&url).expect("failed to parse DATABASE_URL");

    let pool = PoolOptions::new()
        // Postgres' normal connection limit is 100 plus 3 superuser connections
        // We don't want to use the whole cap and there may be fuzziness here due to
        // concurrently running tests anyway.
        .max_connections(20)
        // Immediately close master connections. Tokio's I/O streams don't like hopping runtimes.
        .after_release(|_conn, _| Box::pin(async move { Ok(false) }))
        .connect_lazy_with(master_opts);

    let master_pool = match once_lock_try_insert_polyfill(&MASTER_POOL, pool) {
        Ok(inserted) => inserted,
        Err((existing, pool)) => {
            // Sanity checks.
            assert_eq!(
                existing.connect_options().host,
                pool.connect_options().host,
                "DATABASE_URL changed at runtime, host differs"
            );

            assert_eq!(
                existing.connect_options().database,
                pool.connect_options().database,
                "DATABASE_URL changed at runtime, database differs"
            );

            existing
        }
    };

    let mut conn = master_pool.acquire().await?;

    // language=PostgreSQL
    conn.execute(
        // Explicit lock avoids this latent bug: https://stackoverflow.com/a/29908840
        // I couldn't find a bug on the mailing list for `CREATE SCHEMA` specifically,
        // but a clearly related bug with `CREATE TABLE` has been known since 2007:
        // https://www.postgresql.org/message-id/200710222037.l9MKbCJZ098744%40wwwmaster.postgresql.org
        // magic constant 8318549251334697844 is just 8 ascii bytes 'sqlxtest'.
        r#"
        select pg_advisory_xact_lock(8318549251334697844);

        create schema if not exists _sqlx_test;

        create table if not exists _sqlx_test.databases (
            db_name text primary key,
            test_path text not null,
            created_at timestamptz not null default now()
        );

        create index if not exists databases_created_at 
            on _sqlx_test.databases(created_at);

        create sequence if not exists _sqlx_test.database_ids;

        create table if not exists _sqlx_test.templates (
            template_name text primary key,
            created_at timestamptz not null default now(),
            last_used_at timestamptz not null default now()
        );
    "#,
    )
    .await?;

    let db_name = Postgres::db_name(args);
    do_cleanup(&mut conn, &db_name).await?;

    query(
        r#"
            insert into _sqlx_test.databases(db_name, test_path) values ($1, $2)
        "#,
    )
    .bind(&db_name)
    .bind(args.test_path)
    .execute(&mut *conn)
    .await?;

    let template = match args.migrator {
        Some(migrator) if templates_enabled() => {
            Some(ensure_template(&mut conn, &master_pool.connect_options(), migrator).await?)
        }
        _ => None,
    };

    let create_command = match &template {
        Some(template) => format!("create database {db_name:?} template {template:?}"),
        None => format!("create database {db_name:?}"),
    };
    debug_assert!(create_command.starts_with("create database \""));
    conn.execute(AssertSqlSafe(create_command)).await?;

    Ok(TestContext {
        pool_opts: PoolOptions::new()
            // Don't allow a single test to take all the connections.
            // Most tests shouldn't require more than 5 connections concurrently,
            // or else they're likely doing too much in one test.
            .max_connections(5)
            // Close connections ASAP if left in the idle queue.
            .idle_timeout(Some(Duration::from_secs(1)))
            .parent(master_pool.clone()),
        connect_opts: master_pool
            .connect_options()
            .deref()
            .clone()
            .database(&db_name),
        db_name,
    })
}

/// Whether `SQLX_TEST_TEMPLATES` opts in to template databases (`1` or `true`, case-insensitive).
fn templates_enabled() -> bool {
    dotenvy::var("SQLX_TEST_TEMPLATES").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Derive the template database name from everything that shapes the migrated schema.
fn template_name(migrator: &Migrator) -> String {
    fn field(hasher: &mut Sha512, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    let mut hasher = Sha512::new();

    field(&mut hasher, migrator.table_name.as_bytes());
    hasher.update([migrator.no_tx as u8]);

    hasher.update((migrator.create_schemas.len() as u64).to_le_bytes());
    for schema in migrator.create_schemas.iter() {
        field(&mut hasher, schema.as_bytes());
    }

    hasher.update((migrator.migrations.len() as u64).to_le_bytes());
    for migration in migrator.migrations.iter() {
        hasher.update(migration.version.to_le_bytes());
        field(
            &mut hasher,
            format!("{:?}", migration.migration_type).as_bytes(),
        );
        field(&mut hasher, &migration.checksum);
        hasher.update([migration.no_tx as u8]);
    }

    let hash = hasher.finalize();
    let hash = URL_SAFE.encode(&hash[..36]);
    let name = format!("_sqlx_template_{hash}").replace('-', "_");
    debug_assert!(name.len() == 63);
    name
}

/// Ensure a migrated template database exists for `migrator` and return its name.
///
/// Serialized across processes with a session-level advisory lock, because `CREATE DATABASE`
/// cannot run inside a transaction and so a transaction-level lock would not span it.
async fn ensure_template(
    conn: &mut PgConnection,
    connect_opts: &PgConnectOptions,
    migrator: &Migrator,
) -> Result<String, Error> {
    let template = template_name(migrator);

    // magic constant 8319684163996232044 is just 8 ascii bytes 'sqlxtmpl'.
    query("select pg_advisory_lock(8319684163996232044)")
        .execute(&mut *conn)
        .await?;

    let result = build_template(conn, connect_opts, migrator, &template).await;

    // Always release the lock. If the connection is broken the lock goes away with the session
    // anyway, so only surface the unlock error if the build itself succeeded.
    let unlock = query("select pg_advisory_unlock(8319684163996232044)")
        .execute(&mut *conn)
        .await;

    result?;
    unlock?;

    Ok(template)
}

async fn build_template(
    conn: &mut PgConnection,
    connect_opts: &PgConnectOptions,
    migrator: &Migrator,
    template: &str,
) -> Result<(), Error> {
    let exists: bool = query_scalar(
        r#"
            select exists(select 1 from _sqlx_test.templates where template_name = $1)
                and exists(select 1 from pg_database where datname = $1)
        "#,
    )
    .bind(template)
    .fetch_one(&mut *conn)
    .await?;

    if exists {
        query("update _sqlx_test.templates set last_used_at = now() where template_name = $1")
            .bind(template)
            .execute(&mut *conn)
            .await?;

        return Ok(());
    }

    drop_template(conn, template).await?;

    conn.execute(AssertSqlSafe(format!("create database {template:?}")))
        .await?;

    if let Err(e) = migrate_template(conn, connect_opts, migrator, template).await {
        if let Err(drop_err) = drop_template(conn, template).await {
            eprintln!("could not drop partial template {template:?}: {drop_err}");
        }
        return Err(e);
    }

    query("insert into _sqlx_test.templates(template_name) values ($1)")
        .bind(template)
        .execute(&mut *conn)
        .await?;

    prune_templates(conn, template).await
}

async fn migrate_template(
    conn: &mut PgConnection,
    connect_opts: &PgConnectOptions,
    migrator: &Migrator,
    template: &str,
) -> Result<(), Error> {
    let mut template_conn = connect_opts.clone().database(template).connect().await?;

    let migrated = migrator.run_direct(None, &mut template_conn, false).await;
    let closed = template_conn.close().await;

    migrated?;
    closed?;

    // `CREATE DATABASE ... TEMPLATE` fails while anyone is connected to the source.
    let deadline = Instant::now() + TEMPLATE_IDLE_TIMEOUT;
    loop {
        let connected: i64 = query_scalar(
            "select count(*) from pg_stat_activity where datname = $1 and pid <> pg_backend_pid()",
        )
        .bind(template)
        .fetch_one(&mut *conn)
        .await?;

        if connected == 0 {
            break;
        }

        if Instant::now() >= deadline {
            return Err(Error::Protocol(format!(
                "timed out waiting for connections to template database {template:?} to close"
            )));
        }

        sqlx_core::rt::sleep(Duration::from_millis(50)).await;
    }

    conn.execute(AssertSqlSafe(format!(
        "alter database {template:?} with allow_connections false"
    )))
    .await?;

    Ok(())
}

async fn drop_template(conn: &mut PgConnection, template: &str) -> Result<(), Error> {
    conn.execute(AssertSqlSafe(format!(
        "drop database if exists {template:?}"
    )))
    .await?;
    query("delete from _sqlx_test.templates where template_name = $1")
        .bind(template)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Drop other templates that have not been used recently.
async fn prune_templates(conn: &mut PgConnection, keep: &str) -> Result<(), Error> {
    let stale: Vec<String> = query_scalar(
        r#"
            select template_name from _sqlx_test.templates
            where template_name <> $1
                and last_used_at < now() - make_interval(days => $2)
        "#,
    )
    .bind(keep)
    .bind(TEMPLATE_MAX_AGE_DAYS)
    .fetch_all(&mut *conn)
    .await?;

    for name in &stale {
        match drop_template(conn, name).await {
            Ok(()) => {}
            Err(Error::Database(dbe)) => {
                eprintln!("could not prune template database {name:?}: {dbe}")
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

async fn do_cleanup(conn: &mut PgConnection, db_name: &str) -> Result<(), Error> {
    let delete_db_command = format!("drop database if exists {db_name:?};");
    conn.execute(AssertSqlSafe(delete_db_command)).await?;
    query("delete from _sqlx_test.databases where db_name = $1::text")
        .bind(db_name)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

fn once_lock_try_insert_polyfill<T>(this: &OnceLock<T>, value: T) -> Result<&T, (&T, T)> {
    let mut value = Some(value);
    let res = this.get_or_init(|| value.take().unwrap());
    match value {
        None => Ok(res),
        Some(value) => Err((res, value)),
    }
}
