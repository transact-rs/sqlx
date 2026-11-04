use crate::config::Config;
use crate::opt::{AddMigrationOpts, ConnectOpts, MigrationSourceOpt};
use anyhow::{bail, Context};
use console::style;
use sqlx::migrate::{AppliedMigration, Migrate, MigrateError, MigrationType, Migrator};
use sqlx::Connection;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::fs::{self, File};
use std::path::Path;
use std::time::Duration;

pub async fn add(opts: AddMigrationOpts) -> anyhow::Result<()> {
    let config = opts.config.load_config().await?;

    let source = opts.source.resolve_path(&config);

    fs::create_dir_all(source).context("Unable to create migrations directory")?;

    let migrator = opts.source.resolve(&config).await?;

    let version_prefix = opts.version_prefix(&config, &migrator);

    if opts.reversible(&config, &migrator) {
        create_file(
            source,
            &version_prefix,
            &opts.description,
            MigrationType::ReversibleUp,
        )?;
        create_file(
            source,
            &version_prefix,
            &opts.description,
            MigrationType::ReversibleDown,
        )?;
    } else {
        create_file(
            source,
            &version_prefix,
            &opts.description,
            MigrationType::Simple,
        )?;
    }

    // if the migrations directory is empty
    let has_existing_migrations = fs::read_dir(source)
        .map(|mut dir| dir.next().is_some())
        .unwrap_or(false);

    if !has_existing_migrations {
        let quoted_source = if opts.source.source.is_some() {
            format!("{source:?}")
        } else {
            "".to_string()
        };

        // Provide a link to the current version in case the details change.
        // Patch version is deliberately omitted.
        let version = if let (Some(major), Some(minor)) = (
            // Don't fail if we're not being built by Cargo
            option_env!("CARGO_PKG_VERSION_MAJOR"),
            option_env!("CARGO_PKG_VERSION_MINOR"),
        ) {
            format!("{major}.{minor}")
        } else {
            // If a version isn't available, "latest" is fine.
            "latest".to_string()
        };

        print!(
            r#"
Congratulations on creating your first migration!

Did you know you can embed your migrations in your application binary?
On startup, after creating your database connection or pool, add:

sqlx::migrate!({quoted_source}).run(<&your_pool OR &mut your_connection>).await?;

Note that the compiler won't pick up new migrations if no Rust source files have changed.
You can create a Cargo build script to work around this with `sqlx migrate build-script`.

See: https://docs.rs/sqlx/{version}/sqlx/macro.migrate.html
"#,
        );
    }

    Ok(())
}

fn create_file(
    migration_source: &str,
    file_prefix: &str,
    description: &str,
    migration_type: MigrationType,
) -> anyhow::Result<()> {
    use std::path::PathBuf;

    let mut file_name = file_prefix.to_string();
    file_name.push('_');
    file_name.push_str(&description.replace(' ', "_"));
    file_name.push_str(migration_type.suffix());

    let mut path = PathBuf::new();
    path.push(migration_source);
    path.push(&file_name);

    println!("Creating {}", style(path.display()).cyan());

    let mut file = File::create(&path).context("Failed to create migration file")?;

    std::io::Write::write_all(&mut file, migration_type.file_content().as_bytes())?;

    Ok(())
}

fn short_checksum(checksum: &[u8]) -> String {
    let mut s = String::with_capacity(checksum.len() * 2);
    for b in checksum {
        write!(&mut s, "{b:02x?}").expect("should not fail to write to str");
    }
    s
}

pub async fn info(
    config: &Config,
    migration_source: &MigrationSourceOpt,
    connect_opts: &ConnectOpts,
) -> anyhow::Result<()> {
    let migrator = migration_source.resolve(config).await?;

    let mut conn = crate::connect(config, connect_opts).await?;

    // FIXME: we shouldn't actually be creating anything here
    for schema_name in &config.migrate.create_schemas {
        conn.create_schema_if_not_exists(schema_name).await?;
    }

    conn.ensure_migrations_table(config.migrate.table_name())
        .await?;

    let applied_migrations: HashMap<_, _> = conn
        .list_applied_migrations(config.migrate.table_name())
        .await?
        .into_iter()
        .map(|m| (m.version, m))
        .collect();

    for migration in migrator.iter() {
        if migration.migration_type.is_down_migration() {
            // Skipping down migrations
            continue;
        }

        let applied = applied_migrations.get(&migration.version);

        let (status_msg, mismatched_checksum) = if let Some(applied) = applied {
            if applied.checksum != migration.checksum {
                (style("installed (different checksum)").red(), true)
            } else {
                (style("installed").green(), false)
            }
        } else {
            (style("pending").yellow(), false)
        };

        println!(
            "{}/{} {}",
            style(migration.version).cyan(),
            status_msg,
            migration.description
        );

        if mismatched_checksum {
            println!(
                "applied migration had checksum {}",
                short_checksum(
                    &applied
                        .map(|a| a.checksum.clone())
                        .unwrap_or_else(|| Cow::Owned(vec![]))
                ),
            );
            println!(
                "local migration has checksum {}",
                short_checksum(&migration.checksum)
            )
        }
    }

    let _ = conn.close().await;

    Ok(())
}

fn validate_applied_migrations(
    applied_migrations: &[AppliedMigration],
    migrator: &Migrator,
    ignore_missing: bool,
) -> Result<(), MigrateError> {
    if ignore_missing {
        return Ok(());
    }

    let migrations: HashSet<_> = migrator.iter().map(|m| m.version).collect();

    for applied_migration in applied_migrations {
        if !migrations.contains(&applied_migration.version) {
            return Err(MigrateError::VersionMissing(applied_migration.version));
        }
    }

    Ok(())
}

pub async fn run(
    config: &Config,
    migration_source: &MigrationSourceOpt,
    connect_opts: &ConnectOpts,
    dry_run: bool,
    ignore_missing: bool,
    target_version: Option<i64>,
    skip: bool,
) -> anyhow::Result<()> {
    let migrator = migration_source.resolve(config).await?;

    if let Some(target_version) = target_version {
        if !migrator.version_exists(target_version) {
            bail!(MigrateError::VersionNotPresent(target_version));
        }
    }

    let mut conn = crate::connect(config, connect_opts).await?;

    for schema_name in &config.migrate.create_schemas {
        conn.create_schema_if_not_exists(schema_name).await?;
    }

    conn.ensure_migrations_table(config.migrate.table_name())
        .await?;

    let version = conn.dirty_version(config.migrate.table_name()).await?;
    if let Some(version) = version {
        bail!(MigrateError::Dirty(version));
    }

    let applied_migrations = conn
        .list_applied_migrations(config.migrate.table_name())
        .await?;
    validate_applied_migrations(&applied_migrations, &migrator, ignore_missing)?;

    let latest_version = applied_migrations
        .iter()
        .max_by(|x, y| x.version.cmp(&y.version))
        .map(|migration| migration.version)
        .unwrap_or(0);
    if let Some(target_version) = target_version {
        if target_version < latest_version {
            bail!(MigrateError::VersionTooOld(target_version, latest_version));
        }
    }

    let applied_migrations: HashMap<_, _> = applied_migrations
        .into_iter()
        .map(|m| (m.version, m))
        .collect();

    for migration in migrator.iter() {
        if migration.migration_type.is_down_migration() {
            // Skipping down migrations
            continue;
        }

        match applied_migrations.get(&migration.version) {
            Some(applied_migration) => {
                if migration.checksum != applied_migration.checksum {
                    bail!(MigrateError::VersionMismatch(migration.version));
                }
            }
            None => {
                let exceeds_target =
                    target_version.is_some_and(|target_version| migration.version > target_version);

                let elapsed = if dry_run || exceeds_target {
                    Duration::new(0, 0)
                } else if skip {
                    conn.skip(config.migrate.table_name(), migration).await?;
                    Duration::new(0, 0)
                } else {
                    conn.apply(config.migrate.table_name(), migration).await?
                };
                let text = if exceeds_target {
                    "Skipped"
                } else if dry_run {
                    "Can apply"
                } else if skip {
                    "Skipped on request"
                } else {
                    "Applied"
                };

                println!(
                    "{} {}/{} {} {}",
                    text,
                    style(migration.version).cyan(),
                    style(migration.migration_type.label()).green(),
                    migration.description,
                    style(format!("({elapsed:?})")).dim()
                );
            }
        }
    }

    // Close the connection before exiting:
    // * For MySQL and Postgres this should ensure timely cleanup on the server side,
    //   including decrementing the open connection count.
    // * For SQLite this should checkpoint and delete the WAL file to ensure the migrations
    //   were actually applied to the database file and aren't just sitting in the WAL file.
    let _ = conn.close().await;

    Ok(())
}

pub async fn revert(
    config: &Config,
    migration_source: &MigrationSourceOpt,
    connect_opts: &ConnectOpts,
    dry_run: bool,
    ignore_missing: bool,
    target_version: Option<i64>,
) -> anyhow::Result<()> {
    let migrator = migration_source.resolve(config).await?;

    if let Some(target_version) = target_version {
        if target_version != 0 && !migrator.version_exists(target_version) {
            bail!(MigrateError::VersionNotPresent(target_version));
        }
    }

    let mut conn = crate::connect(config, connect_opts).await?;

    // FIXME: we should not be creating anything here if it doesn't exist
    for schema_name in &config.migrate.create_schemas {
        conn.create_schema_if_not_exists(schema_name).await?;
    }

    conn.ensure_migrations_table(config.migrate.table_name())
        .await?;

    let version = conn.dirty_version(config.migrate.table_name()).await?;
    if let Some(version) = version {
        bail!(MigrateError::Dirty(version));
    }

    let applied_migrations = conn
        .list_applied_migrations(config.migrate.table_name())
        .await?;
    validate_applied_migrations(&applied_migrations, &migrator, ignore_missing)?;

    let latest_version = applied_migrations
        .iter()
        .max_by(|x, y| x.version.cmp(&y.version))
        .map(|migration| migration.version)
        .unwrap_or(0);
    if let Some(target_version) = target_version {
        if target_version > latest_version {
            bail!(MigrateError::VersionTooNew(target_version, latest_version));
        }
    }

    let applied_migrations: HashMap<_, _> = applied_migrations
        .into_iter()
        .map(|m| (m.version, m))
        .collect();

    let mut is_applied = false;
    for migration in migrator.iter().rev() {
        if !migration.migration_type.is_down_migration() {
            // Skipping non down migration
            // This will skip any simple or up migration file
            continue;
        }

        if applied_migrations.contains_key(&migration.version) {
            let skip =
                target_version.is_some_and(|target_version| migration.version <= target_version);

            let elapsed = if dry_run || skip {
                Duration::new(0, 0)
            } else {
                conn.revert(config.migrate.table_name(), migration).await?
            };
            let text = if skip {
                "Skipped"
            } else if dry_run {
                "Can apply"
            } else {
                "Applied"
            };

            println!(
                "{} {}/{} {} {}",
                text,
                style(migration.version).cyan(),
                style(migration.migration_type.label()).green(),
                migration.description,
                style(format!("({elapsed:?})")).dim()
            );

            is_applied = true;

            // Only a single migration will be reverted at a time if no target
            // version is supplied, so we break.
            if target_version.is_none() {
                break;
            }
        }
    }
    if !is_applied {
        println!("No migrations available to revert");
    }

    let _ = conn.close().await;

    Ok(())
}

pub fn build_script(
    config: &Config,
    migration_source: &MigrationSourceOpt,
    force: bool,
) -> anyhow::Result<()> {
    let source = migration_source.resolve_path(config);

    anyhow::ensure!(
        Path::new("Cargo.toml").exists(),
        "must be run in a Cargo project root"
    );

    anyhow::ensure!(
        (force || !Path::new("build.rs").exists()),
        "build.rs already exists; use --force to overwrite"
    );

    let contents = format!(
        r#"// generated by `sqlx migrate build-script`
fn main() {{
    // trigger recompilation when a new migration is added
    println!("cargo:rerun-if-changed={source}");
}}
"#,
    );

    fs::write("build.rs", contents)?;

    println!("Created `build.rs`; be sure to check it into version control!");

    Ok(())
}
