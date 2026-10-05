use sqlx::{query, sqlite::SqliteConnectOptions, ConnectOptions};
use std::str::FromStr;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut conn = SqliteConnectOptions::from_str("sqlite::memory:")?
        .with_regexp()
        .connect()
        .await?;

    // We're not running the migrations here, for the sake of brevity
    // and to confirm that the needed extension was loaded during the
    // CLI migrate operation. It would not be unusual to run the
    // migrations here as well, though, using the database connection
    // we just configured.

    let _ = query!("SELECT 1 AS value WHERE 1 REGEXP '.*'")
        .fetch_one(&mut conn)
        .await?;

    println!("Queries which require the regexp function were successfully executed.");

    Ok(())
}
