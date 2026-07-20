use crate::config::migrate::{DefaultMigrationType, DefaultVersioning};
use crate::config::Config;
use anyhow::Context;
use chrono::Utc;
use clap::{
    builder::{styling::AnsiColor, Styles},
    Args, Parser,
};
#[cfg(feature = "completions")]
use clap_complete::Shell;
use sqlx::migrate::{MigrateError, Migrator, ResolveWith};
use std::env;
use std::ops::{Deref, Not};
use std::path::PathBuf;

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Blue.on_default().bold())
    .usage(AnsiColor::Blue.on_default().bold())
    .literal(AnsiColor::White.on_default())
    .placeholder(AnsiColor::Green.on_default());

#[derive(Parser, Debug)]
#[clap(version, about, author, styles = HELP_STYLES)]
pub struct Opt {
    // https://github.com/launchbadge/sqlx/pull/3724 placed this here,
    // but the intuitive place would be in the arguments for each subcommand.
    #[clap(flatten)]
    pub no_dotenv: NoDotenvOpt,

    #[clap(subcommand)]
    pub command: Command,
}

#[derive(Parser, Debug)]
pub enum Command {
    #[clap(alias = "db")]
    Database(DatabaseOpt),

    /// Generate query metadata to support offline compile-time verification.
    ///
    /// Saves metadata for all invocations of `query!` and related macros to a `.sqlx` directory
    /// in the current directory (or workspace root with `--workspace`), overwriting if needed.
    ///
    /// During project compilation, the absence of the `DATABASE_URL` environment variable or
    /// the presence of `SQLX_OFFLINE` (with a value of `true` or `1`) will constrain the
    /// compile-time verification to only read from the cached query metadata.
    #[clap(alias = "prep")]
    Prepare {
        /// Run in 'check' mode. Exits with 0 if the query metadata is up-to-date. Exits with
        /// 1 if the query metadata needs updating.
        #[clap(long)]
        check: bool,

        /// Prepare query macros in dependencies that exist outside the current crate or workspace.
        #[clap(long)]
        all: bool,

        /// Generate a single workspace-level `.sqlx` folder.
        ///
        /// This option is intended for workspaces where multiple crates use SQLx. If there is only
        /// one, it is better to run `cargo sqlx prepare` without this option inside that crate.
        #[clap(long)]
        workspace: bool,

        /// Arguments to be passed to `cargo rustc ...`.
        #[clap(last = true)]
        args: Vec<String>,

        #[clap(flatten)]
        connect_opts: ConnectOpts,

        #[clap(flatten)]
        config: ConfigOpt,
    },

    #[clap(alias = "mig")]
    Migrate(MigrateOpt),

    #[cfg(feature = "completions")]
    /// Generate shell completions for the specified shell
    Completions { shell: Shell },
}

/// Group of commands for creating and dropping your database.
#[derive(Parser, Debug)]
pub struct DatabaseOpt {
    #[clap(subcommand)]
    pub command: DatabaseCommand,
}

#[derive(Parser, Debug)]
pub enum DatabaseCommand {
    /// Creates the database specified in your DATABASE_URL.
    Create {
        #[clap(flatten)]
        connect_opts: ConnectOpts,

        #[clap(flatten)]
        config: ConfigOpt,
    },

    /// Drops the database specified in your DATABASE_URL.
    Drop {
        #[clap(flatten)]
        confirmation: Confirmation,

        #[clap(flatten)]
        config: ConfigOpt,

        #[clap(flatten)]
        connect_opts: ConnectOpts,

        /// PostgreSQL only: force drops the database.
        #[clap(long, short, default_value = "false")]
        force: bool,
    },

    /// Drops the database specified in your DATABASE_URL, re-creates it, and runs any pending migrations.
    Reset {
        #[clap(flatten)]
        confirmation: Confirmation,

        #[clap(flatten)]
        source: MigrationSourceOpt,

        #[clap(flatten)]
        config: ConfigOpt,

        #[clap(flatten)]
        connect_opts: ConnectOpts,

        /// PostgreSQL only: force drops the database.
        #[clap(long, short, default_value = "false")]
        force: bool,
    },

    /// Creates the database specified in your DATABASE_URL and runs any pending migrations.
    Setup {
        #[clap(flatten)]
        source: MigrationSourceOpt,

        #[clap(flatten)]
        config: ConfigOpt,

        #[clap(flatten)]
        connect_opts: ConnectOpts,
    },
}

/// Group of commands for creating and running migrations.
#[derive(Parser, Debug)]
pub struct MigrateOpt {
    #[clap(subcommand)]
    pub command: MigrateCommand,
}

#[derive(Parser, Debug)]
pub enum MigrateCommand {
    /// Create a new migration with the given description.
    ///
    /// --------------------------------
    ///
    /// Migrations may either be simple, or reversible.
    ///
    /// Reversible migrations can be reverted with `sqlx migrate revert`, simple migrations cannot.
    ///
    /// Reversible migrations are created as a pair of two files with the same filename but
    /// extensions `.up.sql` and `.down.sql` for the up-migration and down-migration, respectively.
    ///
    /// The up-migration should contain the commands to be used when applying the migration,
    /// while the down-migration should contain the commands to reverse the changes made by the
    /// up-migration.
    ///
    /// When writing down-migrations, care should be taken to ensure that they
    /// do not leave the database in an inconsistent state.
    ///
    /// Simple migrations have just `.sql` for their extension and represent an up-migration only.
    ///
    /// Note that reverting a migration is **destructive** and will likely result in data loss.
    /// Reverting a migration will not restore any data discarded by commands in the up-migration.
    ///
    /// It is recommended to always back up the database before running migrations.
    ///
    /// --------------------------------
    ///
    /// For convenience, this command attempts to detect if reversible migrations are in-use.
    ///
    /// If the latest existing migration is reversible, the new migration will also be reversible.
    ///
    /// Otherwise, a simple migration is created.
    ///
    /// This behavior can be overridden by `--simple` or `--reversible`, respectively.
    ///
    /// The default type to use can also be set in `sqlx.toml`.
    ///
    /// --------------------------------
    ///
    /// A version number will be automatically assigned to the migration.
    ///
    /// Migrations are applied in ascending order by version number.
    /// Version numbers do not need to be strictly consecutive.
    ///
    /// The migration process will abort if SQLx encounters a migration with a version number
    /// less than _any_ previously applied migration.
    ///
    /// Migrations should only be created with increasing version number.
    ///
    /// --------------------------------
    ///
    /// For convenience, this command will attempt to detect if sequential versioning is in use,
    /// and if so, continue the sequence.
    ///
    /// Sequential versioning is inferred if:
    ///
    /// * The version numbers of the last two migrations differ by exactly 1, or:
    ///
    /// * only one migration exists and its version number is either 0 or 1.
    ///
    /// Otherwise, timestamp versioning (`YYYYMMDDHHMMSS`) is assumed.
    ///
    /// This behavior can be overridden by `--timestamp` or `--sequential`, respectively.
    ///
    /// The default versioning to use can also be set in `sqlx.toml`.
    Add(AddMigrationOpts),

    /// Run all pending migrations.
    Run {
        #[clap(flatten)]
        source: MigrationSourceOpt,

        #[clap(flatten)]
        config: ConfigOpt,

        /// List all the migrations to be run without applying
        #[clap(long)]
        dry_run: bool,

        #[clap(flatten)]
        ignore_missing: IgnoreMissing,

        #[clap(flatten)]
        connect_opts: ConnectOpts,

        /// Apply migrations up to the specified version. If unspecified, apply all
        /// pending migrations. If already at the target version, then no-op.
        #[clap(long)]
        target_version: Option<i64>,
    },

    /// Override migration state, potentially dangerous operations.
    Override {
        #[clap(subcommand)]
        command: OverrideCommand,
    },

    /// Revert the latest migration with a down file.
    Revert {
        #[clap(flatten)]
        source: MigrationSourceOpt,

        #[clap(flatten)]
        config: ConfigOpt,

        /// List the migration to be reverted without applying
        #[clap(long)]
        dry_run: bool,

        #[clap(flatten)]
        ignore_missing: IgnoreMissing,

        #[clap(flatten)]
        connect_opts: ConnectOpts,

        /// Revert migrations down to the specified version. If unspecified, revert
        /// only the last migration. Set to 0 to revert all migrations. If already
        /// at the target version, then no-op.
        #[clap(long)]
        target_version: Option<i64>,
    },

    /// List all available migrations.
    Info {
        #[clap(flatten)]
        source: MigrationSourceOpt,

        #[clap(flatten)]
        config: ConfigOpt,

        #[clap(flatten)]
        connect_opts: ConnectOpts,
    },

    /// Generate a `build.rs` to trigger recompilation when a new migration is added.
    ///
    /// Must be run in a Cargo project root.
    BuildScript {
        #[clap(flatten)]
        source: MigrationSourceOpt,

        #[clap(flatten)]
        config: ConfigOpt,

        /// Overwrite the build script if it already exists.
        #[clap(long)]
        force: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum OverrideCommand {
    /// Skip all pending migrations without running them.
    Skip {
        #[clap(flatten)]
        source: MigrationSourceOpt,

        #[clap(flatten)]
        config: ConfigOpt,

        #[clap(flatten)]
        connect_opts: ConnectOpts,

        /// List all the migrations to be skipped without marking them as applied.
        #[clap(long)]
        dry_run: bool,

        #[clap(flatten)]
        ignore_missing: IgnoreMissing,

        /// Apply migrations up to the specified version. If unspecified, apply all
        /// pending migrations. If already at the target version, then no-op.
        #[clap(long)]
        target_version: Option<i64>,
    },
}

#[derive(Args, Debug)]
pub struct AddMigrationOpts {
    pub description: String,

    #[clap(flatten)]
    pub source: MigrationSourceOpt,

    #[clap(flatten)]
    pub config: ConfigOpt,

    /// If set, create an up-migration only. Conflicts with `--reversible`.
    #[clap(long, conflicts_with = "reversible")]
    simple: bool,

    /// If set, create a pair of up and down migration files with same version.
    ///
    /// Conflicts with `--simple`.
    #[clap(short, long, conflicts_with = "simple")]
    reversible: bool,

    /// If set, use timestamp versioning for the new migration. Conflicts with `--sequential`.
    ///
    /// Timestamp format: `YYYYMMDDHHMMSS`
    #[clap(short, long, conflicts_with = "sequential")]
    timestamp: bool,

    /// If set, use sequential versioning for the new migration. Conflicts with `--timestamp`.
    #[clap(short, long, conflicts_with = "timestamp")]
    sequential: bool,
}

/// Argument for the migration scripts source.
#[derive(Args, Debug)]
pub struct MigrationSourceOpt {
    /// Path to folder containing migrations.
    ///
    /// Defaults to `migrations/` if not specified, but a different default may be set by `sqlx.toml`.
    #[clap(long)]
    pub source: Option<String>,
}

impl MigrationSourceOpt {
    pub fn resolve_path<'a>(&'a self, config: &'a Config) -> &'a str {
        if let Some(source) = &self.source {
            return source;
        }

        config.migrate.migrations_dir()
    }

    pub async fn resolve(&self, config: &Config) -> Result<Migrator, MigrateError> {
        Migrator::new(ResolveWith(
            self.resolve_path(config),
            config.migrate.to_resolve_config(),
        ))
        .await
    }
}

/// Argument for the database URL.
#[derive(Args, Debug)]
pub struct ConnectOpts {
    #[clap(flatten)]
    pub no_dotenv: NoDotenvOpt,

    /// Location of the DB, by default will be read from the DATABASE_URL env var or `.env` files.
    #[clap(long, short = 'D')]
    pub database_url: Option<String>,

    /// The maximum time, in seconds, to try connecting to the database server before
    /// returning an error.
    #[clap(long, default_value = "10")]
    pub connect_timeout: u64,

    /// Set whether or not to create SQLite databases in Write-Ahead Log (WAL) mode:
    /// https://www.sqlite.org/wal.html
    ///
    /// WAL mode is enabled by default for SQLite databases created by `sqlx-cli`.
    ///
    /// However, if your application sets a `journal_mode` on `SqliteConnectOptions` to something
    /// other than `Wal`, then it will have to take the database file out of WAL mode on connecting,
    /// which requires an exclusive lock and may return a `database is locked` (`SQLITE_BUSY`) error.
    #[cfg(feature = "_sqlite")]
    #[clap(long, action = clap::ArgAction::Set, default_value = "true")]
    pub sqlite_create_db_wal: bool,
}

#[derive(Args, Debug)]
pub struct NoDotenvOpt {
    /// Do not automatically load `.env` files.
    #[clap(long)]
    // Parsing of this flag is actually handled _before_ calling Clap,
    // by `crate::maybe_apply_dotenv()`.
    #[allow(unused)] // TODO: switch to `#[expect]`
    pub no_dotenv: bool,
}

#[derive(Args, Debug)]
pub struct ConfigOpt {
    /// Override the path to the config file.
    ///
    /// Defaults to `sqlx.toml` in the current directory, if it exists.
    ///
    /// Configuration file loading may be bypassed with `--config=/dev/null` on Linux,
    /// or `--config=NUL` on Windows.
    ///
    /// Config file loading is enabled by the `sqlx-toml` feature.
    #[clap(long)]
    pub config: Option<PathBuf>,
}

impl ConnectOpts {
    /// Require a database URL to be provided, otherwise
    /// return an error.
    pub fn expect_db_url(&self) -> anyhow::Result<&str> {
        self.database_url
            .as_deref()
            .context("BUG: database_url not populated")
    }

    /// Populate `database_url` from the environment, if not set.
    pub fn populate_db_url(&mut self, config: &Config) -> anyhow::Result<()> {
        if self.database_url.is_some() {
            return Ok(());
        }

        let var = config.common.database_url_var();

        let context = if var != "DATABASE_URL" {
            " (`common.database-url-var` in `sqlx.toml`)"
        } else {
            ""
        };

        match env::var(var) {
            Ok(url) => {
                if !context.is_empty() {
                    eprintln!("Read database url from `{var}`{context}");
                }

                self.database_url = Some(url)
            }
            Err(env::VarError::NotPresent) => {
                anyhow::bail!("`--database-url` or `{var}`{context} must be set")
            }
            Err(env::VarError::NotUnicode(_)) => {
                anyhow::bail!("`{var}`{context} is not valid UTF-8");
            }
        }

        Ok(())
    }
}

impl ConfigOpt {
    pub async fn load_config(&self) -> anyhow::Result<Config> {
        let path = self.config.clone();

        // Tokio does file I/O on a background task anyway
        tokio::task::spawn_blocking(|| {
            if let Some(path) = path {
                let err_str = format!("error reading config from {path:?}");
                Config::try_from_path(path).context(err_str)
            } else {
                let path = PathBuf::from("sqlx.toml");

                if path.exists() {
                    eprintln!("Found `sqlx.toml` in current directory; reading...");
                    Ok(Config::try_from_path(path)?)
                } else {
                    Ok(Config::default())
                }
            }
        })
        .await
        .context("unexpected error loading config")?
    }
}

/// Argument for automatic confirmation.
#[derive(Args, Copy, Clone, Debug)]
pub struct Confirmation {
    /// Automatic confirmation. Without this option, you will be prompted before dropping
    /// your database.
    #[clap(short)]
    pub yes: bool,
}

/// Argument for ignoring applied migrations that were not resolved.
#[derive(Args, Copy, Clone, Debug)]
pub struct IgnoreMissing {
    /// Ignore applied migrations that are missing in the resolved migrations
    #[clap(long)]
    ignore_missing: bool,
}

impl Deref for IgnoreMissing {
    type Target = bool;

    fn deref(&self) -> &Self::Target {
        &self.ignore_missing
    }
}

impl Not for IgnoreMissing {
    type Output = bool;

    fn not(self) -> Self::Output {
        !self.ignore_missing
    }
}

impl AddMigrationOpts {
    pub fn reversible(&self, config: &Config, migrator: &Migrator) -> bool {
        if self.reversible {
            return true;
        }
        if self.simple {
            return false;
        }

        match config.migrate.defaults.migration_type {
            DefaultMigrationType::Inferred => migrator
                .iter()
                .last()
                .is_some_and(|m| m.migration_type.is_reversible()),
            DefaultMigrationType::Simple => false,
            DefaultMigrationType::Reversible => true,
        }
    }

    pub fn version_prefix(&self, config: &Config, migrator: &Migrator) -> String {
        let default_versioning = &config.migrate.defaults.migration_versioning;

        match (self.timestamp, self.sequential, default_versioning) {
            (true, false, _) | (false, false, DefaultVersioning::Timestamp) => next_timestamp(),
            (false, true, _) | (false, false, DefaultVersioning::Sequential) => fmt_sequential(
                migrator
                    .migrations
                    .last()
                    .map_or(1, |migration| migration.version + 1),
            ),
            (false, false, DefaultVersioning::Inferred) => {
                migrator
                    .migrations
                    .rchunks(2)
                    .next()
                    .and_then(|migrations| {
                        match migrations {
                            [previous, latest] => {
                                // If the latest two versions differ by 1, infer sequential.
                                (latest.version - previous.version == 1)
                                    .then_some(latest.version + 1)
                            }
                            [latest] => {
                                // If only one migration exists and its version is 0 or 1, infer sequential
                                matches!(latest.version, 0 | 1).then_some(latest.version + 1)
                            }
                            _ => unreachable!(),
                        }
                    })
                    .map_or_else(next_timestamp, fmt_sequential)
            }
            (true, true, _) => unreachable!("BUG: Clap should have rejected this case"),
        }
    }
}

fn next_timestamp() -> String {
    Utc::now().format("%Y%m%d%H%M%S").to_string()
}

fn fmt_sequential(version: i64) -> String {
    format!("{version:04}")
}
