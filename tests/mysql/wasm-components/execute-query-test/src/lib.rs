use sqlx::mysql::MySqlConnection;
use sqlx::{Connection, Executor};
use std::env;

async fn run() -> anyhow::Result<()> {
    let database_url = env::var("DATABASE_URL")?;
    let mut conn = MySqlConnection::connect(&database_url).await?;

    let result = conn.execute("DO 1").await?;
    // DO statement affects 0 rows but executes successfully
    assert_eq!(result.rows_affected(), 0);

    conn.close().await?;
    eprintln!("Execute query test passed!");
    Ok(())
}

wasip3::cli::command::export!(Component);

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                if let Err(err) = run().await {
                    eprintln!("Execute query test failed: {err:#}");
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await
    }
}
