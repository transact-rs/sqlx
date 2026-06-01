use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, Row};
use std::env;

async fn run() -> anyhow::Result<()> {
    let database_url = env::var("DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;

    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS wasi_postgres_todos (
            id BIGSERIAL PRIMARY KEY,
            description TEXT NOT NULL,
            done BOOL NOT NULL DEFAULT FALSE
        )
        "#,
    )
    .await?;

    let row = sqlx::query("INSERT INTO wasi_postgres_todos (description) VALUES ($1) RETURNING id")
        .bind("Test todo")
        .fetch_one(&pool)
        .await?;
    let id: i64 = row.try_get("id")?;

    let row = sqlx::query("SELECT description, done FROM wasi_postgres_todos WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await?;
    let description: &str = row.try_get("description")?;
    let done: bool = row.try_get("done")?;
    assert_eq!(description, "Test todo");
    assert!(!done);

    let update_result = sqlx::query("UPDATE wasi_postgres_todos SET done = TRUE WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await?;
    assert_eq!(update_result.rows_affected(), 1);

    let delete_result = sqlx::query("DELETE FROM wasi_postgres_todos WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await?;
    assert_eq!(delete_result.rows_affected(), 1);

    eprintln!("Postgres pool CRUD test passed!");
    Ok(())
}

wasip3::cli::command::export!(Component);

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                if let Err(err) = run().await {
                    eprintln!("Postgres pool CRUD test failed: {err:#}");
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await
    }
}
