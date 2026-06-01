use sqlx::postgres::PgConnection;
use sqlx::Connection;
use std::env;

async fn run() -> anyhow::Result<()> {
    let database_url = env::var("DATABASE_URL")?;
    let mut conn = PgConnection::connect(&database_url).await?;
    conn.ping().await?;
    conn.close().await?;
    eprintln!("Postgres connect test passed!");
    Ok(())
}

wasip3::cli::command::export!(Component);

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                if let Err(err) = run().await {
                    eprintln!("Postgres connect test failed: {err:#}");
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await
    }
}
