//! Demonstrates how to configure a connection pool using `SqlitePoolOptions`.
//!
//! Run with:
//!
//! ```not_rust
//! cargo run -p sqlx-example-sqlite-pool-configuration
//! ```

use sqlx::sqlite::SqlitePoolOptions;
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), sqlx::Error> {
    let pool = SqlitePoolOptions::new()
        // Set the maximum number of connections the pool should maintain.
        .max_connections(5)
        // Set the minimum number of idle connections the pool should maintain.
        .min_connections(1)
        // Set the maximum amount of time to wait for a connection to become available.
        .acquire_timeout(Duration::from_secs(3))
        // Set the maximum idle duration for individual connections.
        // Connections that sit idle longer than this are closed.
        .idle_timeout(Duration::from_secs(60 * 10))
        // Set the maximum lifetime of individual connections.
        // Connections older than this are closed regardless of idle time.
        .max_lifetime(Duration::from_secs(60 * 30))
        // Connect to an in-memory SQLite database.
        .connect(":memory:")
        .await?;

    // Create a table and insert some data to verify the pool works.
    sqlx::query("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .execute(&pool)
        .await?;

    sqlx::query("INSERT INTO users (name) VALUES (?)")
        .bind("Alice")
        .execute(&pool)
        .await?;

    sqlx::query("INSERT INTO users (name) VALUES (?)")
        .bind("Bob")
        .execute(&pool)
        .await?;

    // Query the data back.
    let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM users ORDER BY id")
        .fetch_all(&pool)
        .await?;

    for (id, name) in &rows {
        println!("user: id={id}, name={name}");
    }

    // Print pool statistics.
    println!("\nPool statistics:");
    println!("  size:          {}", pool.size());
    println!("  idle:          {}", pool.num_idle());

    pool.close().await;
    println!("  closed:        true");

    Ok(())
}
