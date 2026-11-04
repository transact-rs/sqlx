use sqlx::migrate::Migrator;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{Sqlite, SqliteConnection};
use sqlx::Executor;
use sqlx::Row;
use std::path::Path;

#[sqlx::test(migrations = false)]
async fn simple(mut conn: PoolConnection<Sqlite>) -> anyhow::Result<()> {
    clean_up(&mut conn).await?;

    let migrator = Migrator::new(Path::new("tests/sqlite/migrations_simple")).await?;

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
async fn reversible(mut conn: PoolConnection<Sqlite>) -> anyhow::Result<()> {
    clean_up(&mut conn).await?;

    let migrator = Migrator::new(Path::new("tests/sqlite/migrations_reversible")).await?;

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
async fn skip(mut conn: PoolConnection<Sqlite>) -> anyhow::Result<()> {
    clean_up(&mut conn).await?;
    let migrator = Migrator::new(Path::new("tests/sqlite/migrations_reversible")).await?;

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
async fn no_tx(mut conn: PoolConnection<Sqlite>) -> anyhow::Result<()> {
    clean_up(&mut conn).await?;
    let migrator = Migrator::new(Path::new("tests/sqlite/migrations_no_tx")).await?;

    // run migration
    migrator.run(&mut conn).await?;

    Ok(())
}

#[sqlx::test(migrations = false)]
async fn no_tx_reversible(mut conn: PoolConnection<Sqlite>) -> anyhow::Result<()> {
    clean_up(&mut conn).await?;

    let migrator = Migrator::new(Path::new("tests/sqlite/migrations_no_tx_reversible")).await?;

    // run migration
    migrator.run(&mut conn).await?;

    // check outcome
    let res: String = conn.fetch_one("PRAGMA JOURNAL_MODE").await?.get(0);
    assert_eq!(res, "wal".to_string());

    // roll back
    migrator.undo(&mut conn, -1).await?;

    // check outcome
    let res: String = conn.fetch_one("PRAGMA JOURNAL_MODE").await?.get(0);
    assert_eq!(res, "delete".to_string());

    Ok(())
}

/// Ensure that we have a clean initial state.
async fn clean_up(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    conn.execute("DROP TABLE migrations_simple_test").await.ok();
    conn.execute("DROP TABLE migrations_reversible_test")
        .await
        .ok();
    conn.execute("DROP TABLE _sqlx_migrations").await.ok();

    Ok(())
}
