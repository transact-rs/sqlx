use sqlx::postgres::{PgConnectOptions, PgConnection, PgSslMode};
use sqlx::Connection;
use std::env;
use std::str::FromStr;

async fn run() -> anyhow::Result<()> {
    let database_url = env::var("DATABASE_URL")?;
    let opts = PgConnectOptions::from_str(&database_url)?.ssl_mode(PgSslMode::Require);

    let mut conn = PgConnection::connect_with(&opts).await?;
    conn.ping().await?;
    conn.close().await?;
    eprintln!("Postgres TLS connect test passed!");
    Ok(())
}

wasip3::cli::command::export!(Component);

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                if let Err(err) = run().await {
                    eprintln!("Postgres TLS connect test failed: {err:#}");
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await
    }
}
