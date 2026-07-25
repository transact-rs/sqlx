use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Executor;
use sqlx::Row;

// --- Helpers ---

async fn setup_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .min_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS bench (
            id INTEGER PRIMARY KEY,
            i32_val INTEGER NOT NULL,
            i64_val BIGINT NOT NULL,
            f64_val REAL NOT NULL,
            text_val TEXT NOT NULL,
            blob_val BLOB NOT NULL
        );
        "#,
    )
    .await
    .unwrap();

    // Seed 10,000 rows for large result benchmarks.
    // Using a single INSERT with a recursive CTE to avoid 10K round-trips.
    pool.execute(
        r#"
        WITH RECURSIVE cnt(id) AS (
            SELECT 1
            UNION ALL
            SELECT id + 1 FROM cnt WHERE id < 10000
        )
        INSERT INTO bench (id, i32_val, i64_val, f64_val, text_val, blob_val)
        SELECT
            id,
            id AS i32_val,
            id * 1000 AS i64_val,
            id * 1.5 AS f64_val,
            'row_' || id AS text_val,
            randomblob(64) AS blob_val
        FROM cnt;
        "#,
    )
    .await
    .unwrap();

    pool
}

// --- Benchmarks ---

/// #1: Establish a new connection.
async fn bench_connect() {
    let mut conn = sqlx::sqlite::SqliteConnection::connect("sqlite::memory:")
        .await
        .unwrap();
    let _ = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&mut conn)
        .await
        .unwrap();
}

/// #2: Checkout an idle connection from the pool.
async fn bench_pool_checkout(pool: &SqlitePool) {
    let mut conn = pool.acquire().await.unwrap();
    let _ = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
}

/// #3: Ping a connection.
async fn bench_ping(pool: &SqlitePool) {
    let mut conn = pool.acquire().await.unwrap();
    conn.ping().await.unwrap();
}

/// #4: Small query, small results (TechEmpower single-row query).
/// Single SELECT returning a handful of columns, one row.
async fn bench_query_small(pool: &SqlitePool) {
    let _row: (i64, i32, String) = sqlx::query_as("SELECT 1, 2, 'hello'")
        .fetch_one(pool)
        .await
        .unwrap();
}

/// #5: Small query, huge results (10K rows, 5+ columns with String and BLOB).
async fn bench_query_large(pool: &SqlitePool) {
    let mut rows = sqlx::query("SELECT id, i32_val, i64_val, f64_val, text_val, blob_val FROM bench")
        .fetch(pool);

    let mut count = 0u64;
    while let Some(row) = rows.try_next().await.unwrap() {
        let _id: i64 = row.get("id");
        let _i32: i32 = row.get("i32_val");
        let _i64: i64 = row.get("i64_val");
        let _f64: f64 = row.get("f64_val");
        let _text: &str = row.get("text_val");
        let _blob: &[u8] = row.get("blob_val");
        count += 1;
    }
    assert_eq!(count, 10_000);
}

// --- Criterion harness ---

fn criterion_benchmarks(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let pool = runtime.block_on(setup_pool());

    let mut group = c.benchmark_group("sqlx_sqlite");

    group.bench_function("connect", |b| {
        b.to_async(&runtime).iter(|| bench_connect());
    });

    group.bench_function("pool_checkout", |b| {
        b.to_async(&runtime).iter(|| bench_pool_checkout(&pool));
    });

    group.bench_function("ping", |b| {
        b.to_async(&runtime).iter(|| bench_ping(&pool));
    });

    group.bench_function("query_small", |b| {
        b.to_async(&runtime).iter(|| bench_query_small(&pool));
    });

    group.bench_function("query_large_10k", |b| {
        b.to_async(&runtime).iter(|| bench_query_large(&pool));
    });

    group.finish();
}

criterion_group!(benches, criterion_benchmarks);
criterion_main!(benches);
