use sqlx::mysql::MySqlPoolOptions;
use sqlx::{Executor, Row};
use std::env;

async fn run() -> anyhow::Result<()> {
    let database_url = env::var("DATABASE_URL")?;
    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;

    // Create table
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS wasi_todos (
            id BIGINT PRIMARY KEY AUTO_INCREMENT,
            description TEXT NOT NULL,
            done BOOL NOT NULL DEFAULT FALSE
        )
        "#,
    )
    .await?;

    // Insert
    let insert_result = sqlx::query("INSERT INTO wasi_todos (description) VALUES (?)")
        .bind("Test todo")
        .execute(&pool)
        .await?;
    assert!(insert_result.last_insert_id() > 0);

    // Select
    let row = sqlx::query("SELECT id, description, done FROM wasi_todos WHERE id = ?")
        .bind(insert_result.last_insert_id())
        .fetch_one(&pool)
        .await?;
    let description: &str = row.try_get("description")?;
    assert_eq!(description, "Test todo");

    // Update
    let update_result = sqlx::query("UPDATE wasi_todos SET done = TRUE WHERE id = ?")
        .bind(insert_result.last_insert_id())
        .execute(&pool)
        .await?;
    assert_eq!(update_result.rows_affected(), 1);

    // Delete
    let delete_result = sqlx::query("DELETE FROM wasi_todos WHERE id = ?")
        .bind(insert_result.last_insert_id())
        .execute(&pool)
        .await?;
    assert_eq!(delete_result.rows_affected(), 1);

    eprintln!("Pool CRUD test passed!");
    Ok(())
}

wasip3::cli::command::export!(Component);

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                if let Err(err) = run().await {
                    eprintln!("Pool CRUD test failed: {err:#}");
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await
    }
}
