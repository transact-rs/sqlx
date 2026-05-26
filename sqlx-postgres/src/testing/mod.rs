use std::future::Future;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use sqlx_core::connection::Connection;
use sqlx_core::query_builder::QueryBuilder;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_core::HashMap;

use crate::error::Error;
use crate::executor::Executor;
use crate::pool::{Pool, PoolOptions};
use crate::query::query;
use crate::{PgConnectOptions, PgConnection, Postgres};

pub(crate) use sqlx_core::testing::*;

static MASTER_POOLS: LazyLock<Mutex<HashMap<&'static str, Pool<Postgres>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Automatically delete any databases created before the start of the test binary.

impl TestSupport for Postgres {
    fn test_context(
        args: &TestArgs,
    ) -> impl Future<Output = Result<TestContext<Self>, Error>> + Send + '_ {
        test_context(args)
    }

    async fn cleanup_test(args: &TestArgs) -> Result<(), Error> {
        let db_name = Self::db_name(args);

        let master_pool = get_master_pool(args.database_url_var);
        let mut conn = master_pool.acquire().await?;

        do_cleanup(&mut conn, &db_name).await
    }

    async fn cleanup_test_dbs() -> Result<Option<usize>, Error> {
        let url = dotenvy::var("DATABASE_URL").expect("DATABASE_URL must be set");

        let count = Self::cleanup_test_dbs_by_url(&url).await?;

        Ok(count)
    }

    async fn cleanup_test_dbs_by_url(url: &str) -> Result<Option<usize>, Error> {
        let mut conn = PgConnection::connect(url).await?;

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
    let database_url_var = args.database_url_var;

    let url = dotenvy::var(database_url_var).expect("DATABASE_URL must be set");

    let master_opts = PgConnectOptions::from_str(&url).expect("failed to parse DATABASE_URL");

    let pool = PoolOptions::new()
        // Postgres' normal connection limit is 100 plus 3 superuser connections
        // We don't want to use the whole cap and there may be fuzziness here due to
        // concurrently running tests anyway.
        .max_connections(20)
        // Immediately close master connections. Tokio's I/O streams don't like hopping runtimes.
        .after_release(|_conn, _| Box::pin(async move { Ok(false) }))
        .connect_lazy_with(master_opts);

    let master_pool = match try_insert_polyfill(database_url_var, pool) {
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

    let create_command = format!("create database {db_name:?}");
    debug_assert!(create_command.starts_with("create database \""));
    conn.execute(AssertSqlSafe(create_command)).await?;

    eprintln!("created database {db_name}");

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

async fn do_cleanup(conn: &mut PgConnection, db_name: &str) -> Result<(), Error> {
    let delete_db_command = format!("drop database if exists {db_name:?};");
    conn.execute(AssertSqlSafe(delete_db_command)).await?;
    query("delete from _sqlx_test.databases where db_name = $1::text")
        .bind(db_name)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

fn get_master_pool(database_url_var: &'static str) -> Pool<Postgres> {
    let guard = MASTER_POOLS
        .lock()
        .expect("failed to acquire lock of master pools");
    guard
        .get(database_url_var)
        .expect("cleanup_test() invoked outside `#[sqlx::test]`")
        .clone()
}

fn try_insert_polyfill(
    database_url_var: &'static str,
    pool: Pool<Postgres>,
) -> Result<Pool<Postgres>, (Pool<Postgres>, Pool<Postgres>)> {
    let mut guard = MASTER_POOLS
        .lock()
        .expect("failed to acquire lock of master pools");
    let master_pool = guard.get(database_url_var);

    match master_pool {
        None => {
            guard.insert(database_url_var, pool.clone());
            Ok(pool)
        }
        Some(master_pool) => Err((master_pool.clone(), pool)),
    }
}
