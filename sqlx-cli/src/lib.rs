//! # SQLx CLI
//!
//! Command-line utility for the [SQLx](https://github.com/launchbadge/sqlx) ecosystem.
//!
//! This crate provides the core logic for the `sqlx` command-line interface, enabling database management,
//! migrations, and offline query preparation for Rust projects using SQLx.
//!
//! ### Note: Semver Exempt API
//! The API of this crate is not meant for general use and does *not* follow Semantic Versioning.
//! The only crate that follows Semantic Versioning in the project is the `sqlx` crate itself.
//! If you are building a custom SQLx driver, you should pin an exact version for `sqlx-cli` to
//! avoid breakages:
//!
//! ```toml
//! sqlx-cli = { version = "=0.9.0" }
//! ```
//!
//! And then make releases in lockstep with `sqlx-cli`. We recommend all driver crates, in-tree
//! or otherwise, use the same version numbers as `sqlx-cli` to avoid confusion.

use std::future::Future;
use std::io;
use std::time::Duration;

use futures_util::TryFutureExt;

use sqlx::AnyConnection;
use tokio::{select, signal};

use crate::opt::{Command, ConnectOpts, DatabaseCommand, MigrateCommand};

pub mod database;
pub mod metadata;
// mod migration;
// mod migrator;
#[cfg(feature = "completions")]
pub mod completions;
pub mod migrate;
pub mod opt;
pub mod prepare;
pub mod revalidate;

pub use crate::opt::Opt;

pub use sqlx::_unstable::config::{self, Config};

/// Check arguments for `--no-dotenv` _before_ Clap parsing, and apply `.env` if not set.
pub fn maybe_apply_dotenv() {
    if std::env::args().any(|arg| arg == "--no-dotenv") {
        return;
    }

    if let Err(e) = dotenvy::dotenv() {
        if !e.not_found() {
            eprintln!("Warning: error loading `.env` file: {e:?}");
        }
    }
}

pub async fn run(opt: Opt) -> anyhow::Result<()> {
    // This `select!` is here so that when the process receives a `SIGINT` (CTRL + C),
    // the futures currently running on this task get dropped before the program exits.
    // This is currently necessary for the consumers of the `dialoguer` crate to restore
    // the user's terminal if the process is interrupted while a dialog is being displayed.

    let ctrlc_fut = signal::ctrl_c();
    let do_run_fut = do_run(opt);

    select! {
        biased;
        _ = ctrlc_fut => {
            Ok(())
        },
        do_run_outcome = do_run_fut => {
            do_run_outcome
        }
    }
}

async fn do_run(opt: Opt) -> anyhow::Result<()> {
    match opt.command {
        Command::Migrate(migrate) => match migrate.command {
            MigrateCommand::Add(opts) => migrate::add(opts).await?,
            MigrateCommand::Run {
                source,
                config,
                dry_run,
                ignore_missing,
                mut connect_opts,
                target_version,
            } => {
                let config = config.load_config().await?;

                connect_opts.populate_db_url(&config)?;

                migrate::run(
                    &config,
                    &source,
                    &connect_opts,
                    dry_run,
                    *ignore_missing,
                    target_version,
                )
                .await?
            }
            MigrateCommand::Revert {
                source,
                config,
                dry_run,
                ignore_missing,
                mut connect_opts,
                target_version,
            } => {
                let config = config.load_config().await?;

                connect_opts.populate_db_url(&config)?;

                migrate::revert(
                    &config,
                    &source,
                    &connect_opts,
                    dry_run,
                    *ignore_missing,
                    target_version,
                )
                .await?
            }
            MigrateCommand::Info {
                source,
                config,
                mut connect_opts,
            } => {
                let config = config.load_config().await?;

                connect_opts.populate_db_url(&config)?;

                migrate::info(&config, &source, &connect_opts).await?
            }
            MigrateCommand::BuildScript {
                source,
                config,
                force,
            } => {
                let config = config.load_config().await?;

                migrate::build_script(&config, &source, force)?
            }
        },

        Command::Database(database) => match database.command {
            DatabaseCommand::Create {
                config,
                mut connect_opts,
            } => {
                let config = config.load_config().await?;

                connect_opts.populate_db_url(&config)?;
                database::create(&connect_opts).await?
            }
            DatabaseCommand::Drop {
                confirmation,
                config,
                mut connect_opts,
                force,
            } => {
                let config = config.load_config().await?;

                connect_opts.populate_db_url(&config)?;
                database::drop(&connect_opts, !confirmation.yes, force).await?
            }
            DatabaseCommand::Reset {
                confirmation,
                source,
                config,
                mut connect_opts,
                force,
            } => {
                let config = config.load_config().await?;

                connect_opts.populate_db_url(&config)?;
                database::reset(&config, &source, &connect_opts, !confirmation.yes, force).await?
            }
            DatabaseCommand::Setup {
                source,
                config,
                mut connect_opts,
            } => {
                let config = config.load_config().await?;

                connect_opts.populate_db_url(&config)?;
                database::setup(&config, &source, &connect_opts).await?
            }
        },

        Command::Prepare {
            check,
            all,
            workspace,
            mut connect_opts,
            args,
            config,
        } => {
            let config = config.load_config().await?;
            connect_opts.populate_db_url(&config)?;
            prepare::run(&config, check, all, workspace, connect_opts, args).await?
        }

        #[cfg(feature = "completions")]
        Command::Completions { shell } => completions::run(shell),

        Command::Revalidate {
            mut connect_opts,
            config,
            database,
        } => {
            let config = config.load_config().await?;
            connect_opts.populate_db_url(&config)?;
            revalidate::run_revalidate(connect_opts, database.as_deref()).await?;
        }
    };

    Ok(())
}

/// Attempt to connect to the database server, retrying up to `ops.connect_timeout`.
async fn connect(config: &Config, opts: &ConnectOpts) -> anyhow::Result<AnyConnection> {
    retry_connect_errors(opts, move |url| {
        AnyConnection::connect_with_driver_config(url, &config.drivers)
    })
    .await
}

/// Attempt an operation that may return errors like `ConnectionRefused`,
/// retrying up until `ops.connect_timeout`.
///
/// The closure is passed `&ops.database_url` for easy composition.
async fn retry_connect_errors<'a, F, Fut, T>(
    opts: &'a ConnectOpts,
    mut connect: F,
) -> anyhow::Result<T>
where
    F: FnMut(&'a str) -> Fut,
    Fut: Future<Output = sqlx::Result<T>> + 'a,
{
    let db_url = opts.expect_db_url()?;

    backoff::future::retry(
        backoff::ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(Duration::from_secs(opts.connect_timeout)))
            .build(),
        || {
            connect(db_url).map_err(|e| -> backoff::Error<anyhow::Error> {
                if let sqlx::Error::Io(ref ioe) = e {
                    match ioe.kind() {
                        io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted => {
                            return backoff::Error::transient(e.into());
                        }
                        _ => (),
                    }
                }

                backoff::Error::permanent(e.into())
            })
        },
    )
    .await
}
