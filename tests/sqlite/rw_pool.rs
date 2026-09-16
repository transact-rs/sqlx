use sqlx::sqlite::{SqliteConnectOptions, SqliteRwPool, SqliteRwPoolOptions};
use sqlx::{Acquire, Row};
use std::str::FromStr;
use tempfile::TempDir;

fn temp_db_opts() -> (SqliteConnectOptions, TempDir) {
    let dir = TempDir::new().unwrap();
    let filepath = dir.path().join("test.db");
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", filepath.display()))
        .unwrap()
        .create_if_missing(true);
    (opts, dir)
}

// ─── Basic connectivity ────────────────────────────────────────────────────────

#[sqlx_macros::test]
async fn rw_pool_connect_and_close() -> anyhow::Result<()> {
    let (opts, _dir) = temp_db_opts();
    let pool = SqliteRwPool::connect_with(opts).await?;

    assert!(!pool.is_closed());
    assert!(pool.num_writers() > 0);

    pool.close().await;
    assert!(pool.is_closed());

    Ok(())
}

// ─── Transactions always use writer ────────────────────────────────────────────

#[sqlx_macros::test]
async fn rw_pool_transactions_use_writer() -> anyhow::Result<()> {
    let (opts, _dir) = temp_db_opts();
    let pool = SqliteRwPool::connect_with(opts).await?;

    sqlx::query("CREATE TABLE items (id INTEGER PRIMARY KEY, val TEXT)")
        .execute(&pool)
        .await?;

    // begin() should use writer
    let mut tx = pool.begin().await?;

    sqlx::query("INSERT INTO items (val) VALUES ('a')")
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO items (val) VALUES ('b')")
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    let count = sqlx::query_scalar::<sqlx::Sqlite, i64>("SELECT count(*) FROM items")
        .fetch_one(&pool)
        .await?;

    assert_eq!(count, 2);

    // Test rollback
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO items (val) VALUES ('c')")
        .execute(&mut *tx)
        .await?;
    tx.rollback().await?;

    let count = sqlx::query_scalar::<sqlx::Sqlite, i64>("SELECT count(*) FROM items")
        .fetch_one(&pool)
        .await?;

    assert_eq!(count, 2); // still 2, rollback worked

    pool.close().await;
    Ok(())
}

// ─── Acquire routes to writer ──────────────────────────────────────────────────

#[sqlx_macros::test]
async fn rw_pool_acquire_routes_to_writer() -> anyhow::Result<()> {
    let (opts, _dir) = temp_db_opts();
    let pool = SqliteRwPool::connect_with(opts).await?;

    // Acquire via the Acquire trait should give writer
    let mut conn = (&pool).acquire().await?;

    sqlx::query("CREATE TABLE acq_test (id INTEGER PRIMARY KEY)")
        .execute(&mut *conn)
        .await?;

    sqlx::query("INSERT INTO acq_test (id) VALUES (1)")
        .execute(&mut *conn)
        .await?;

    drop(conn);

    let row = sqlx::query("SELECT id FROM acq_test")
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i32, _>("id"), 1);

    pool.close().await;
    Ok(())
}

// ─── Explicit reader/writer access ─────────────────────────────────────────────

#[sqlx_macros::test]
async fn rw_pool_explicit_reader_writer() -> anyhow::Result<()> {
    let (opts, _dir) = temp_db_opts();
    let pool = SqliteRwPool::connect_with(opts).await?;

    // Writer: create and populate table
    let mut writer = pool.acquire_writer().await?;
    sqlx::query("CREATE TABLE expl (id INTEGER PRIMARY KEY, val TEXT)")
        .execute(&mut *writer)
        .await?;
    sqlx::query("INSERT INTO expl (val) VALUES ('hello')")
        .execute(&mut *writer)
        .await?;
    drop(writer);

    // Reader: can read
    let mut reader = pool.acquire_reader().await?;
    let rows = sqlx::query("SELECT val FROM expl")
        .fetch_all(&mut *reader)
        .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<String, _>("val"), "hello");
    drop(reader);

    pool.close().await;
    Ok(())
}

// ─── Reader rejects writes ─────────────────────────────────────────────────────

#[sqlx_macros::test]
async fn rw_pool_reader_rejects_writes() -> anyhow::Result<()> {
    let (opts, _dir) = temp_db_opts();
    let pool = SqliteRwPool::connect_with(opts).await?;

    // Setup
    sqlx::query("CREATE TABLE ro_test (id INTEGER PRIMARY KEY)")
        .execute(&pool)
        .await?;

    // Attempting to write via an explicit reader should fail
    let mut reader = pool.acquire_reader().await?;
    let result = sqlx::query("INSERT INTO ro_test (id) VALUES (1)")
        .execute(&mut *reader)
        .await;

    assert!(result.is_err(), "write on read-only connection should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("readonly") || err.contains("read-only") || err.contains("SQLITE_READONLY"),
        "expected readonly error, got: {err}"
    );

    drop(reader);
    pool.close().await;
    Ok(())
}

// ─── Concurrent reads don't block ──────────────────────────────────────────────

#[sqlx_macros::test]
async fn rw_pool_concurrent_reads() -> anyhow::Result<()> {
    let (opts, _dir) = temp_db_opts();
    let pool = SqliteRwPoolOptions::new()
        .max_readers(4)
        .connect_with(opts)
        .await?;

    sqlx::query("CREATE TABLE conc (id INTEGER PRIMARY KEY, val TEXT)")
        .execute(pool.writer())
        .await?;

    for i in 0..10 {
        sqlx::query("INSERT INTO conc (val) VALUES (?)")
            .bind(format!("item_{i}"))
            .execute(pool.writer())
            .await?;
    }

    // Spawn multiple concurrent reads via explicit reader pool
    let mut handles = Vec::new();
    for _ in 0..4 {
        let reader = pool.reader().clone();
        handles.push(sqlx_core::rt::spawn(async move {
            let rows = sqlx::query("SELECT count(*) as cnt FROM conc")
                .fetch_one(&reader)
                .await
                .unwrap();
            rows.get::<i64, _>("cnt")
        }));
    }

    for handle in handles {
        let count = handle.await;
        assert_eq!(count, 10);
    }

    pool.close().await;
    Ok(())
}

// ─── Pool size introspection ───────────────────────────────────────────────────

#[sqlx_macros::test]
async fn rw_pool_size_introspection() -> anyhow::Result<()> {
    let (opts, _dir) = temp_db_opts();
    let pool = SqliteRwPoolOptions::new()
        .max_readers(3)
        .connect_with(opts)
        .await?;

    // Writer pool has exactly 1 connection
    assert_eq!(pool.num_writers(), 1);

    // Reader pool may have 1 initially (pools lazily create connections)
    assert!(pool.num_readers() >= 1);

    pool.close().await;
    Ok(())
}
