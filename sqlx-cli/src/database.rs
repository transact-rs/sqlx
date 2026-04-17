use crate::opt::{ConnectOpts, MigrationSourceOpt};
use crate::{migrate, Config};
use console::style;
use sqlx::any::Any;
use sqlx::migrate::MigrateDatabase;
use std::io::{self, BufRead, Write};
use tokio::task;

pub async fn create(connect_opts: &ConnectOpts) -> anyhow::Result<()> {
    // NOTE: only retry the idempotent action.
    // We're assuming that if this succeeds, then any following operations should also succeed.
    let exists = crate::retry_connect_errors(connect_opts, Any::database_exists).await?;

    if !exists {
        #[cfg(feature = "_sqlite")]
        sqlx::sqlite::CREATE_DB_WAL.store(
            connect_opts.sqlite_create_db_wal,
            std::sync::atomic::Ordering::Release,
        );

        Any::create_database(connect_opts.expect_db_url()?).await?;
    }

    Ok(())
}

pub async fn drop(connect_opts: &ConnectOpts, confirm: bool, force: bool) -> anyhow::Result<()> {
    if confirm && !ask_to_continue_drop(connect_opts.expect_db_url()?.to_owned()).await {
        return Ok(());
    }

    // NOTE: only retry the idempotent action.
    // We're assuming that if this succeeds, then any following operations should also succeed.
    let exists = crate::retry_connect_errors(connect_opts, Any::database_exists).await?;

    if exists {
        if force {
            Any::force_drop_database(connect_opts.expect_db_url()?).await?;
        } else {
            Any::drop_database(connect_opts.expect_db_url()?).await?;
        }
    }

    Ok(())
}

pub async fn reset(
    config: &Config,
    migration_source: &MigrationSourceOpt,
    connect_opts: &ConnectOpts,
    confirm: bool,
    force: bool,
) -> anyhow::Result<()> {
    drop(connect_opts, confirm, force).await?;
    setup(config, migration_source, connect_opts).await
}

pub async fn setup(
    config: &Config,
    migration_source: &MigrationSourceOpt,
    connect_opts: &ConnectOpts,
) -> anyhow::Result<()> {
    create(connect_opts).await?;
    migrate::run(config, migration_source, connect_opts, false, false, None).await
}

async fn ask_to_continue_drop(db_url: String) -> bool {
    task::spawn_blocking(move || {
        let stderr = io::stderr();
        let mut stderr_lock = stderr.lock();
        let _ = write!(
            stderr_lock,
            "Drop database at {}? (y/N): ",
            style(&db_url).cyan()
        );
        let _ = stderr_lock.flush();
        std::mem::drop(stderr_lock);

        let stdin = io::stdin();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => false,
            Ok(_) => parse_drop_response(&line),
        }
    })
    .await
    .expect("Confirm thread panicked")
}

fn parse_drop_response(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes")
}

#[cfg(test)]
mod tests {
    use super::parse_drop_response;

    #[test]
    fn parse_drop_response_accepts_yes() {
        for input in ["y", "Y", "yes", "YES", "Yes", "y\n", "yes\r\n", " yes "] {
            assert!(
                parse_drop_response(input),
                "expected yes for input {input:?}"
            );
        }
    }

    #[test]
    fn parse_drop_response_rejects_everything_else() {
        for input in [
            "", "\n", "\r\n", "n", "N", "no", "NO", "maybe", " ", "xyz", "yep",
        ] {
            assert!(
                !parse_drop_response(input),
                "expected no for input {input:?}"
            );
        }
    }
}
