use std::future::Future;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use sqlx_core::connection::Connection;
use sqlx_core::query_builder::QueryBuilder;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::sql_str::AssertSqlSafe;

use crate::error::Error;
use crate::executor::Executor;
use crate::pool::{Pool, PoolOptions};
use crate::query::query;
use crate::{PgConnectOptions, PgConnection, Postgres};

pub(crate) use sqlx_core::testing::*;

static MASTER_POOL: TestMasterPool<Postgres> = TestMasterPool::new();

impl TestSupport for Postgres {
    fn test_context(
        args: &TestArgs,
    ) -> impl Future<Output = Result<TestContext<Self>, Error>> + Send + '_ {
        test_context(args)
    }

    async fn cleanup_test(db_name: &str) -> Result<(), Error> {
        let mut conn = MASTER_POOL.acquire().await?;

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

    let mut conn = MASTER_POOL.connect(&master_opts).await?;

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
    .execute(&mut **conn)
    .await?;

    let create_command = format!("create database {db_name:?}");
    debug_assert!(create_command.starts_with("create database \""));
    conn.execute(AssertSqlSafe(create_command)).await?;

    Ok(TestContext {
        pool_opts: PoolOptions::new()
            .max_connections(args.max_connections)
            // Close connections ASAP if left in the idle queue.
            .idle_timeout(Some(Duration::from_secs(1))),
        connect_opts: master_opts.database(&db_name),
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
