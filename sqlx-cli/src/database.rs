use crate::opt::{ConnectOpts, MigrationSourceOpt};
use crate::{migrate, Config};
use console::{style, Term};
use dialoguer::Confirm;
use sqlx::any::Any;
use sqlx::migrate::MigrateDatabase;
use std::{io, mem};
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
    // If the setup operation is cancelled while we are waiting for the user to decide whether
    // or not to drop the database, this will restore the terminal's cursor to its normal state.
    struct RestoreCursorGuard {
        disarmed: bool,
    }

    impl Drop for RestoreCursorGuard {
        fn drop(&mut self) {
            if !self.disarmed {
                Term::stderr().show_cursor().unwrap()
            }
        }
    }

    let mut guard = RestoreCursorGuard { disarmed: false };

    let decision_result = task::spawn_blocking(move || {
        Confirm::new()
            .with_prompt(format!("Drop database at {}?", style(&db_url).cyan()))
            .wait_for_newline(true)
            .default(false)
            .show_default(true)
            .interact()
    })
    .await
    .expect("Confirm thread panicked");
    match decision_result {
        Ok(decision) => {
            guard.disarmed = true;
            decision
        }
        Err(dialoguer::Error::IO(err)) if err.kind() == io::ErrorKind::Interrupted => {
            // Sometimes CTRL + C causes this error to be returned
            mem::drop(guard);
            false
        }
        Err(err) => {
            mem::drop(guard);
            panic!("Confirm dialog failed with {err}")
        }
    }
}
