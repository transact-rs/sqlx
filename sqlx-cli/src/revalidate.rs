use std::str::FromStr;

use anyhow::bail;
use console::style;
use serde::Serialize;
use sqlx::any::AnyConnectOptions;
use sqlx::Connection;
use sqlx::{Database, Describe, Executor, MySql, Postgres, SqlStr, Sqlite};

use crate::opt::ConnectOpts;
use crate::prepare::glob_query_files;

/// Offline query data.
#[derive(Clone, serde::Deserialize)]
pub struct DynQueryData {
    pub db_name: String,
    pub query: String,
    pub describe: serde_json::Value,
    pub hash: String,
}

pub async fn run_revalidate(
    connect_opts: ConnectOpts,
    database: Option<&str>,
) -> anyhow::Result<()> {
    let Some(database_url) = &connect_opts.database_url else {
        bail!("DATABASE_URL must be set!");
    };

    let database = match database {
        Some(database) => database.to_lowercase(),
        None => {
            let url = AnyConnectOptions::from_str(database_url)?;
            url.database_url.scheme().to_lowercase()
        }
    };

    match database.as_str() {
        #[cfg(feature = "mysql")]
        "mysql" => do_run::<MySql>(database_url).await,
        #[cfg(feature = "postgres")]
        "postgres" => do_run::<Postgres>(database_url).await,
        #[cfg(feature = "sqlite")]
        "sqlite" => do_run::<Sqlite>(database_url).await,
        database => bail!("Unknown database: '{database}'"),
    }
}

async fn do_run<DB: Database>(database_url: &str) -> anyhow::Result<()>
where
    Describe<DB>: Serialize,
    for<'ex> &'ex mut DB::Connection: Executor<'ex, Database = DB>,
{
    let mut connection = DB::Connection::connect(database_url).await?;

    let files = glob_query_files(".sqlx")?;
    if files.is_empty() {
        println!("{} no queries found", style("warning:").yellow());
        return Ok(());
    }

    for file in files {
        println!(
            "{} re-validating query file {}",
            style("info:").blue(),
            file.display()
        );
        let expected_config = tokio::fs::read_to_string(&file).await?;
        let config: DynQueryData = serde_json::from_str(&expected_config)?;

        let sql_str = config.query;
        let description: Describe<DB> =
            Executor::describe(&mut connection, SqlStr::from_static(sql_str.leak()))
                .await
                .unwrap();
        let description = serde_json::to_value(description)?;

        if dbg!(description) != dbg!(config.describe) {
            bail!(
                "Query result for query {} is not up-to-date!",
                file.display()
            );
        }
    }

    Ok(())
}
