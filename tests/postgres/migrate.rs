use sqlx::migrate::{MigrateError, Migration, MigrationType, Migrator};
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnection, PgPool, Postgres};
use sqlx::Executor;
use sqlx::Row;
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, SqlSafeStr};
use std::path::Path;

#[sqlx::test(migrations = false)]
async fn simple(mut conn: PoolConnection<Postgres>) -> anyhow::Result<()> {
    clean_up(&mut conn).await?;

    let migrator = Migrator::new(Path::new("tests/postgres/migrations_simple")).await?;

    // run migration
    migrator.run(&mut conn).await?;

    // check outcome
    let res: String = conn
        .fetch_one("SELECT some_payload FROM migrations_simple_test")
        .await?
        .get(0);
    assert_eq!(res, "110_suffix");

    // running it a 2nd time should still work
    migrator.run(&mut conn).await?;

    Ok(())
}

#[sqlx::test(migrations = false)]
async fn reversible(mut conn: PoolConnection<Postgres>) -> anyhow::Result<()> {
    clean_up(&mut conn).await?;

    let migrator = Migrator::new(Path::new("tests/postgres/migrations_reversible")).await?;

    // run migration
    migrator.run(&mut conn).await?;

    // check outcome
    let res: i64 = conn
        .fetch_one("SELECT some_payload FROM migrations_reversible_test")
        .await?
        .get(0);
    assert_eq!(res, 101);

    // roll back nothing (last version)
    migrator.undo(&mut conn, 20220721125033).await?;

    // check outcome
    let res: i64 = conn
        .fetch_one("SELECT some_payload FROM migrations_reversible_test")
        .await?
        .get(0);
    assert_eq!(res, 101);

    // roll back one version
    migrator.undo(&mut conn, 20220721124650).await?;

    // check outcome
    let res: i64 = conn
        .fetch_one("SELECT some_payload FROM migrations_reversible_test")
        .await?
        .get(0);
    assert_eq!(res, 100);

    Ok(())
}

#[sqlx::test(migrations = false)]
async fn skip(mut conn: PoolConnection<Postgres>) -> anyhow::Result<()> {
    clean_up(&mut conn).await?;
    let migrator = Migrator::new(Path::new("tests/postgres/migrations_reversible")).await?;

    // get to the state of after the first migration manually
    let sql = include_str!("migrations_reversible/20220721124650_add_table.up.sql");
    let statements: Vec<&str> = sql.split(';').filter(|s| !s.trim().is_empty()).collect();
    for statement in statements {
        conn.execute(statement).await?;
    }

    // skip first migration
    migrator.skip(&mut conn, Some(20220721124650)).await?;

    // check outcome
    let res: i64 = conn
        .fetch_one("SELECT some_payload FROM migrations_reversible_test")
        .await?
        .get(0);
    assert_eq!(res, 100);

    // run remaining migration
    migrator.run(&mut conn).await?;

    // check outcome
    let res: i64 = conn
        .fetch_one("SELECT some_payload FROM migrations_reversible_test")
        .await?
        .get(0);
    assert_eq!(res, 101);

    // roll back one version
    migrator.undo(&mut conn, 20220721124650).await?;

    // check outcome
    let res: i64 = conn
        .fetch_one("SELECT some_payload FROM migrations_reversible_test")
        .await?
        .get(0);
    assert_eq!(res, 100);

    Ok(())
}

#[sqlx::test(migrations = false)]
async fn no_tx(mut conn: PoolConnection<Postgres>) -> anyhow::Result<()> {
    clean_up(&mut conn).await?;
    let migrator = Migrator::new(Path::new("tests/postgres/migrations_no_tx")).await?;

    // run migration
    migrator.run(&mut conn).await?;

    // check outcome
    let res: String = conn
        .fetch_one("SELECT datname FROM pg_database WHERE datname = 'test_db'")
        .await?
        .get(0);

    assert_eq!(res, "test_db");

    Ok(())
}

/// A migration that creates a schema named after the connecting role makes that schema shadow
/// `public` in the default search path of every session that starts afterwards. An unqualified
/// reference to `_sqlx_migrations` then resolves to the new, empty schema.
///
/// The migrator must report the ambiguity instead of creating a second migrations table and
/// applying every migration a second time.
#[sqlx::test(migrations = false)]
async fn migrations_table_shadowed_by_role_schema(pool: PgPool) -> anyhow::Result<()> {
    let connect_options = pool.connect_options();

    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&pool)
        .await?;

    let migrator = Migrator::with_migrations(vec![
        Migration::new(
            1,
            "create role schema".into(),
            MigrationType::Simple,
            AssertSqlSafe(format!(r#"CREATE SCHEMA "{role}""#)).into_sql_str(),
            false,
        ),
        Migration::new(
            2,
            "add table".into(),
            MigrationType::Simple,
            AssertSqlSafe(format!(
                r#"CREATE TABLE "{role}".migrations_shadowed_test (id INT PRIMARY KEY)"#
            ))
            .into_sql_str(),
            false,
        ),
    ]);

    // The schema only shadows `public` for sessions that start after it exists,
    // so each run needs its own connection.
    let mut conn = connect_options.connect().await?;
    migrator.run(&mut conn).await?;
    conn.close().await?;

    let mut conn = connect_options.connect().await?;
    let error = migrator
        .run(&mut conn)
        .await
        .expect_err("second run did not detect the shadowed migrations table");
    conn.close().await?;

    assert!(
        matches!(error, MigrateError::AmbiguousMigrationsTable { .. }),
        "unexpected error: {error:?}"
    );

    // The second run must not have left an empty migrations table behind in the new schema.
    let tracking_tables: Vec<String> = sqlx::query_scalar(
        "SELECT schemaname::text FROM pg_catalog.pg_tables \
         WHERE tablename = '_sqlx_migrations' ORDER BY schemaname",
    )
    .fetch_all(&pool)
    .await?;

    assert_eq!(tracking_tables, ["public"]);

    Ok(())
}

/// An explicitly chosen table name is the user's responsibility, so the shadowing check is
/// skipped for it and migrations keep running against the name as written.
#[sqlx::test(migrations = false)]
async fn qualified_migrations_table_ignores_role_schema(pool: PgPool) -> anyhow::Result<()> {
    let connect_options = pool.connect_options();

    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&pool)
        .await?;

    let mut migrator = Migrator::with_migrations(vec![Migration::new(
        1,
        "create role schema".into(),
        MigrationType::Simple,
        AssertSqlSafe(format!(r#"CREATE SCHEMA "{role}""#)).into_sql_str(),
        false,
    )]);
    migrator.dangerous_set_table_name("public._sqlx_migrations");

    let mut conn = connect_options.connect().await?;
    migrator.run(&mut conn).await?;
    conn.close().await?;

    // A qualified name cannot move, so the second run is a no-op.
    let mut conn = connect_options.connect().await?;
    migrator.run(&mut conn).await?;
    conn.close().await?;

    let tracking_tables: Vec<String> = sqlx::query_scalar(
        "SELECT schemaname::text FROM pg_catalog.pg_tables \
         WHERE tablename = '_sqlx_migrations' ORDER BY schemaname",
    )
    .fetch_all(&pool)
    .await?;

    assert_eq!(tracking_tables, ["public"]);

    Ok(())
}

/// Ensure that we have a clean initial state.
async fn clean_up(conn: &mut PgConnection) -> anyhow::Result<()> {
    conn.execute("DROP DATABASE IF EXISTS test_db").await.ok();
    conn.execute("DROP TABLE migrations_simple_test").await.ok();
    conn.execute("DROP TABLE migrations_reversible_test")
        .await
        .ok();
    conn.execute("DROP TABLE _sqlx_migrations").await.ok();

    Ok(())
}
