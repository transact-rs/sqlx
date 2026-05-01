use std::future::Future;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use crate::error::Error;
use crate::executor::Executor;
use crate::pool::{Pool, PoolOptions};
use crate::query::query;
use crate::{MySql, MySqlConnectOptions, MySqlConnection, MySqlDatabaseError};
use sqlx_core::connection::Connection;
use sqlx_core::query_builder::QueryBuilder;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::sql_str::AssertSqlSafe;

pub(crate) use sqlx_core::testing::*;

// Using a blocking `OnceLock` here because the critical sections are short.
static MASTER_POOL: OnceLock<Pool<MySql>> = OnceLock::new();

impl TestSupport for MySql {
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

        let mut conn = MySqlConnection::connect(&url).await?;

        let delete_db_names: Vec<String> = query_scalar("select db_name from _sqlx_test_databases")
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

        if deleted_db_names.is_empty() {
            return Ok(None);
        }

        let mut query = QueryBuilder::new("delete from _sqlx_test_databases where db_name in (");

        let mut separated = query.separated(",");

        for db_name in &deleted_db_names {
            separated.push_bind(db_name);
        }

        query.push(")").build().execute(&mut conn).await?;

        let _ = conn.close().await;
        Ok(Some(delete_db_names.len()))
    }

    async fn snapshot(_conn: &mut Self::Connection) -> Result<FixtureSnapshot<Self>, Error> {
        // TODO: I want to get the testing feature out the door so this will have to wait,
        // but I'm keeping the code around for now because I plan to come back to it.
        todo!()
    }
}

async fn test_context(args: &TestArgs) -> Result<TestContext<MySql>, Error> {
    let url = dotenvy::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let master_opts = MySqlConnectOptions::from_str(&url).expect("failed to parse DATABASE_URL");

    let pool = PoolOptions::new()
        // MySql's normal connection limit is 150 plus 1 superuser connection
        // We don't want to use the whole cap and there may be fuzziness here due to
        // concurrently running tests anyway.
        .max_connections(20)
        // Immediately close master connections. Tokio's I/O streams don't like hopping runtimes.
        .after_release(|_conn, _| Box::pin(async move { Ok(false) }))
        .connect_lazy_with(master_opts.clone());

    let master_pool = once_lock_try_insert_polyfill(&MASTER_POOL, pool)
        .unwrap_or_else(|(existing, _pool)| existing);

    let mut conn = master_pool.acquire().await?;

    cleanup_old_dbs(&mut conn).await?;

    // language=MySQL
    conn.execute(
        r#"
        create table if not exists _sqlx_test_databases (
            db_name text not null,
            test_path text not null,
            created_at timestamp not null default current_timestamp,
            -- BLOB/TEXT columns can only be used as index keys with a prefix length:
            -- https://dev.mysql.com/doc/refman/8.4/en/column-indexes.html#column-indexes-prefix
            primary key(db_name(63))
        );
    "#,
    )
    .await?;

    let db_name = MySql::db_name(args);
    do_cleanup(&mut conn, &db_name).await?;

    query("insert into _sqlx_test_databases(db_name, test_path) values (?, ?)")
        .bind(&db_name)
        .bind(args.test_path)
        .execute(&mut *conn)
        .await?;

    conn.execute(AssertSqlSafe(format!("create database {db_name}")))
        .await?;

    eprintln!("created database {db_name}");

    Ok(TestContext {
        pool_opts: PoolOptions::new()
            // Don't allow a single test to take all the connections.
            // Most tests shouldn't require more than 5 connections concurrently,
            // or else they're likely doing too much in one test.
            .max_connections(5)
            // Close connections ASAP if left in the idle queue.
            .idle_timeout(Some(Duration::from_secs(1))),
        connect_opts: master_opts.database(&db_name),
        db_name,
    })
}

async fn do_cleanup(conn: &mut MySqlConnection, db_name: &str) -> Result<(), Error> {
    let delete_db_command = format!("drop database if exists {db_name};");
    conn.execute(AssertSqlSafe(delete_db_command)).await?;
    query("delete from _sqlx_test_databases where db_name = ?")
        .bind(db_name)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

async fn cleanup_old_dbs(conn: &mut MySqlConnection) -> Result<(), Error> {
    let res: Result<Vec<u64>, Error> = query_scalar("select db_id from _sqlx_test_databases")
        .fetch_all(&mut *conn)
        .await;

    let db_ids = match res {
        Ok(db_ids) => db_ids,
        Err(e) => {
            if let Some(dbe) = e.as_database_error() {
                match dbe.downcast_ref::<MySqlDatabaseError>().number() {
                    // Column `db_id` does not exist:
                    // https://dev.mysql.com/doc/mysql-errors/8.0/en/server-error-reference.html#error_er_bad_field_error
                    //
                    // The table has already been migrated.
                    1054 => return Ok(()),
                    // Table `_sqlx_test_databases` does not exist.
                    // No cleanup needed.
                    // https://dev.mysql.com/doc/mysql-errors/8.0/en/server-error-reference.html#error_er_no_such_table
                    1146 => return Ok(()),
                    _ => (),
                }
            }

            return Err(e);
        }
    };

    // Drop old-style test databases.
    for id in db_ids {
        match conn
            .execute(AssertSqlSafe(format!(
                "drop database if exists _sqlx_test_database_{id}"
            )))
            .await
        {
            Ok(_deleted) => (),
            // Assume a database error just means the DB is still in use.
            Err(Error::Database(dbe)) => {
                eprintln!("could not clean old test database _sqlx_test_database_{id}: {dbe}");
            }
            // Bubble up other errors
            Err(e) => return Err(e),
        }
    }

    conn.execute("drop table if exists _sqlx_test_databases")
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
