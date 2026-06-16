//! SQLx + MySQL **connection pool** example targeting `wasm32-wasip2`.
//!
//! Runs on a current-thread Tokio runtime. On `wasm32-wasip2` Tokio's `net`
//! driver works under `--cfg tokio_unstable` (set in `../.cargo/config.toml`).
//!
//! Build & run:
//!
//! ```sh
//! cargo build --target wasm32-wasip2 --release
//! wasmtime run -S inherit-network --env DATABASE_URL \
//!     target/wasm32-wasip2/release/sqlx-wasip2-mysql.wasm
//! ```

use sqlx::mysql::MySqlPoolOptions;
use sqlx::Row;

// `flavor = "current_thread"` because `wasm32-wasip2` does not have threads.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:password@localhost:3306/sqlx".to_owned());

    let pool = MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("failed to connect pool");

    // Fresh schema (safe to re-run).
    sqlx::query("DROP TABLE IF EXISTS wasip2_votes")
        .execute(&pool)
        .await
        .expect("drop table");
    sqlx::query(
        "CREATE TABLE wasip2_votes (id BIGINT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(64) NOT NULL, votes BIGINT NOT NULL)",
    )
    .execute(&pool)
    .await
    .expect("create table");

    // Seed a few rows inside a transaction, using bind parameters.
    let mut tx = pool.begin().await.expect("begin");
    for (name, votes) in [("alice", 5_i64), ("bob", 3), ("carol", 4)] {
        sqlx::query("INSERT INTO wasip2_votes (name, votes) VALUES (?, ?)")
            .bind(name)
            .bind(votes)
            .execute(&mut *tx)
            .await
            .expect("insert");
    }
    tx.commit().await.expect("commit");

    // Read them back, ranked highest-first.
    let rows = sqlx::query("SELECT name, votes FROM wasip2_votes ORDER BY votes DESC")
        .fetch_all(&pool)
        .await
        .expect("select");
    for row in &rows {
        let name: String = row.get("name");
        let votes: i64 = row.get("votes");
        println!("{name}: {votes}");
    }

    // Aggregate (`SUM(bigint)` is `DECIMAL` in MySQL, so cast back to a signed int).
    let total: i64 = sqlx::query_scalar("SELECT CAST(SUM(votes) AS SIGNED) FROM wasip2_votes")
        .fetch_one(&pool)
        .await
        .expect("sum");
    println!("total votes: {total}");

    let version: String = sqlx::query_scalar("SELECT VERSION()")
        .fetch_one(&pool)
        .await
        .expect("version query failed");
    println!("connected via pool to: {version}");

    pool.close().await;
}
