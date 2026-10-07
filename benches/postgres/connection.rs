use criterion::{criterion_group, criterion_main, Criterion};
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};
use std::cell::RefCell;

const LARGE_RESULT_ROWS: i64 = 10_000;

fn db_url() -> String {
    dotenvy::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

async fn setup_conn() -> PgConnection {
    let mut conn = PgConnection::connect(&db_url()).await.unwrap();

    conn.execute(
        "CREATE TEMP TABLE bench_data (
            id    BIGINT  PRIMARY KEY,
            name  TEXT    NOT NULL,
            data  BYTEA   NOT NULL,
            val1  BIGINT  NOT NULL,
            val2  FLOAT8  NOT NULL
        )",
    )
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO bench_data (id, name, data, val1, val2)
         SELECT
             n,
             'row_' || n,
             decode(lpad('', 32, '0'), 'hex'),
             n,
             n::float8
         FROM generate_series(1, $1) AS n",
    )
    .bind(LARGE_RESULT_ROWS)
    .execute(&mut conn)
    .await
    .unwrap();

    conn
}

// ── async helpers ────────────────────────────────────────────────────────────

async fn do_ping(conn: &RefCell<PgConnection>) {
    conn.borrow_mut().ping().await.unwrap();
}

async fn do_query_small(conn: &RefCell<PgConnection>) {
    let mut guard = conn.borrow_mut();
    let row: (i64, String, Vec<u8>, i64, f64) =
        sqlx::query_as("SELECT id, name, data, val1, val2 FROM bench_data WHERE id = 1")
            .fetch_one(&mut *guard)
            .await
            .unwrap();
    std::hint::black_box(row);
}

async fn do_query_large(conn: &RefCell<PgConnection>) {
    let mut guard = conn.borrow_mut();
    let rows: Vec<(i64, String, Vec<u8>, i64, f64)> =
        sqlx::query_as("SELECT id, name, data, val1, val2 FROM bench_data")
            .fetch_all(&mut *guard)
            .await
            .unwrap();
    std::hint::black_box(rows);
}

async fn do_pool_checkout(pool: &sqlx::PgPool) {
    let _conn = pool.acquire().await.unwrap();
}

// ── criterion functions ───────────────────────────────────────────────────────

fn bench_new_connection(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = db_url();
    c.bench_function("new_connection", |b| {
        // Time only `connect`; close gracefully (sends `Terminate`) outside the
        // measured section so we don't leave abandoned sockets on the server
        // across thousands of iterations.
        b.to_async(&runtime).iter_custom(|iters| {
            let url = url.as_str();
            async move {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let start = std::time::Instant::now();
                    let conn = PgConnection::connect(url).await.unwrap();
                    total += start.elapsed();
                    conn.close().await.unwrap();
                }
                total
            }
        });
    });
}

fn bench_pool_checkout(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let pool = runtime
        .block_on(
            PgPoolOptions::new()
                .min_connections(1)
                .max_connections(1)
                // Disable the on-acquire ping (`test_before_acquire` defaults to
                // `true`); otherwise each iteration pays *two* round-trips: a ping
                // on acquire and a ping on release. Note the pool still does a
                // mandatory on-release ping when the guard is dropped, so this
                // measures a full borrow+return cycle, not a purely in-process
                // checkout.
                .test_before_acquire(false)
                .connect(&db_url()),
        )
        .unwrap();

    c.bench_function("pool_checkout", |b| {
        b.to_async(&runtime).iter(|| do_pool_checkout(&pool));
    });
}

fn bench_ping(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let conn = RefCell::new(runtime.block_on(PgConnection::connect(&db_url())).unwrap());

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
