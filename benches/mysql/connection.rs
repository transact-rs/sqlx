use criterion::{criterion_group, criterion_main, Criterion};
use sqlx::mysql::{MySqlConnection, MySqlPoolOptions};
use sqlx::{Connection, Executor};
use std::cell::RefCell;

const LARGE_RESULT_ROWS: i64 = 10_000;

fn db_url() -> String {
    dotenvy::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

async fn setup_conn() -> MySqlConnection {
    let mut conn = MySqlConnection::connect(&db_url()).await.unwrap();

    conn.execute(
        "CREATE TEMPORARY TABLE bench_data (
            id    BIGINT       NOT NULL PRIMARY KEY,
            name  TEXT         NOT NULL,
            data  BLOB         NOT NULL,
            val1  BIGINT       NOT NULL,
            val2  DOUBLE       NOT NULL
        )",
    )
    .await
    .unwrap();

    // MySQL's recursive CTE default limit is 1000; raise it for this session.
    conn.execute("SET cte_max_recursion_depth = 10000")
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO bench_data (id, name, data, val1, val2)
         WITH RECURSIVE gen(n) AS (
             SELECT 1
             UNION ALL
             SELECT n + 1 FROM gen WHERE n < ?
         )
         SELECT n, CONCAT('row_', n), UNHEX(LPAD('', 32, '0')), n, n FROM gen",
    )
    .bind(LARGE_RESULT_ROWS)
    .execute(&mut conn)
    .await
    .unwrap();

    conn
}

// ── async helpers ────────────────────────────────────────────────────────────

async fn do_ping(conn: &RefCell<MySqlConnection>) {
    conn.borrow_mut().ping().await.unwrap();
}

async fn do_query_small(conn: &RefCell<MySqlConnection>) {
    let mut guard = conn.borrow_mut();
    let _: (i64, String, Vec<u8>, i64, f64) =
        sqlx::query_as("SELECT id, name, data, val1, val2 FROM bench_data WHERE id = 1")
            .fetch_one(&mut *guard)
            .await
            .unwrap();
}

async fn do_query_large(conn: &RefCell<MySqlConnection>) {
    let mut guard = conn.borrow_mut();
    let _: Vec<(i64, String, Vec<u8>, i64, f64)> =
        sqlx::query_as("SELECT id, name, data, val1, val2 FROM bench_data")
            .fetch_all(&mut *guard)
            .await
            .unwrap();
}

async fn do_pool_checkout(pool: &sqlx::MySqlPool) {
    let _conn = pool.acquire().await.unwrap();
}

// ── criterion functions ───────────────────────────────────────────────────────

fn bench_new_connection(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = db_url();
    c.bench_function("new_connection", |b| {
        b.to_async(&runtime).iter(|| async {
            drop(MySqlConnection::connect(&url).await.unwrap());
        });
    });
}

fn bench_pool_checkout(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let pool = runtime
        .block_on(
            MySqlPoolOptions::new()
                .min_connections(1)
                .max_connections(1)
                .connect(&db_url()),
        )
        .unwrap();

    c.bench_function("pool_checkout", |b| {
        b.to_async(&runtime).iter(|| do_pool_checkout(&pool));
    });
}

fn bench_ping(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let conn = RefCell::new(
        runtime
            .block_on(MySqlConnection::connect(&db_url()))
            .unwrap(),
    );

    c.bench_function("ping", |b| {
        b.to_async(&runtime).iter(|| do_ping(&conn));
    });
}

fn bench_query_small(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let conn = RefCell::new(runtime.block_on(setup_conn()));

    c.bench_function("query_small_result", |b| {
        b.to_async(&runtime).iter(|| do_query_small(&conn));
    });
}

fn bench_query_large(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let conn = RefCell::new(runtime.block_on(setup_conn()));

    c.bench_function("query_large_result", |b| {
        b.to_async(&runtime).iter(|| do_query_large(&conn));
    });
}

criterion_group!(
    benches,
    bench_new_connection,
    bench_pool_checkout,
    bench_ping,
    bench_query_small,
    bench_query_large,
);
criterion_main!(benches);
