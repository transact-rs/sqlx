use sqlx::postgres::PgConnection;
use sqlx::Connection;
use std::env;

async fn run() -> anyhow::Result<()> {
    let database_url = env::var("DATABASE_URL")?;
    let mut conn = PgConnection::connect(&database_url).await?;

    let value: i32 = sqlx::query_scalar("SELECT $1::INT4 + $2::INT4")
        .bind(2_i32)
        .bind(3_i32)
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(value, 5);

    conn.close().await?;
    eprintln!("Postgres prepared query test passed!");
    Ok(())
}

wasip3::cli::command::export!(Component);

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                if let Err(err) = run().await {
                    eprintln!("Postgres prepared query test failed: {err:#}");
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await
    }
}
