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
    migrate::run(
        config,
        migration_source,
        connect_opts,
        false,
        false,
        None,
        false,
    )
    .await
}

async fn ask_to_continue_drop(db_url: String) -> bool {
    // Plain line-based prompt rather than dialoguer::Confirm. dialoguer puts
    // the terminal into raw mode for the y/N toggle even with
    // wait_for_newline(true), which means keypresses never echo and the whole
    // prompt gets repainted on every flip. That's confusing in general and
    // hostile to screen-reader users (#4236). Reading a line of stdin echoes
    // input the way users expect and doesn't need a cursor-restore guard.
    let prompt = format!("Drop database at {}? (y/N): ", style(&db_url).cyan());

    let decision = task::spawn_blocking(move || -> io::Result<bool> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(prompt.as_bytes())?;
        stderr.flush()?;

        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        let answer = line.trim();
        Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
    })
    .await
    .expect("drop-confirm thread panicked");

    match decision {
        Ok(d) => d,
        Err(err) if err.kind() == io::ErrorKind::Interrupted => false,
        Err(err) => panic!("Confirm dialog failed with {err}"),
    }
}
