use sqlx::mysql::MySqlConnection;
use sqlx::Connection;
use std::env;

async fn run() -> anyhow::Result<()> {
    let database_url = env::var("DATABASE_URL")?;
    let mut conn = MySqlConnection::connect(&database_url).await?;

    // MySQL returns DOUBLE for arithmetic, so use f64 or cast to INT
    let value: i64 = sqlx::query_scalar("SELECT CAST(? + ? AS SIGNED)")
        .bind(2_i32)
        .bind(3_i32)
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(value, 5);

    conn.close().await?;
    eprintln!("Prepared query test passed!");
    Ok(())
}

wasip3::cli::command::export!(Component);

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                if let Err(err) = run().await {
                    eprintln!("Prepared query test failed: {err:#}");
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await
    }
}
