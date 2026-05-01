use crate::acquire::Acquire;
use crate::migrate::{AppliedMigration, Migrate, MigrateError, Migration, MigrationSource};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::slice;

/// A resolved set of migrations, ready to be run.
///
/// Can be constructed statically using `migrate!()` or at runtime using [`Migrator::new()`].
#[derive(Debug)]
// Forbids `migrate!()` from constructing this:
// #[non_exhaustive]
pub struct Migrator {
    // NOTE: these fields are semver-exempt and may be changed or removed in any future version.
    // These have to be public for `migrate!()` to be able to initialize them in an implicitly
    // const-promotable context. A `const fn` constructor isn't implicitly const-promotable.
    #[doc(hidden)]
    pub migrations: Cow<'static, [Migration]>,
    #[doc(hidden)]
    pub ignore_missing: bool,
    #[doc(hidden)]
    pub locking: bool,
    #[doc(hidden)]
    pub no_tx: bool,
    #[doc(hidden)]
    pub table_name: Cow<'static, str>,

    #[doc(hidden)]
    pub create_schemas: Cow<'static, [Cow<'static, str>]>,
}

impl Migrator {
    #[doc(hidden)]
    pub const DEFAULT: Migrator = Migrator {
        migrations: Cow::Borrowed(&[]),
        ignore_missing: false,
        no_tx: false,
        locking: true,
        table_name: Cow::Borrowed("_sqlx_migrations"),
        create_schemas: Cow::Borrowed(&[]),
    };

    /// Creates a new instance with the given source.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use sqlx_core::migrate::MigrateError;
    /// # fn main() -> Result<(), MigrateError> {
    /// # sqlx::__rt::test_block_on(async move {
    /// # use sqlx_core::migrate::Migrator;
    /// use std::path::Path;
    ///
    /// // Read migrations from a local folder: ./migrations
    /// let m = Migrator::new(Path::new("./migrations")).await?;
    /// # Ok(())
    /// # })
    /// # }
    /// ```
    /// See [MigrationSource] for details on structure of the `./migrations` directory.
    pub async fn new<'s, S>(source: S) -> Result<Self, MigrateError>
    where
        S: MigrationSource<'s>,
    {
        Ok(Self {
            migrations: Cow::Owned(source.resolve().await.map_err(MigrateError::Source)?),
            ..Self::DEFAULT
        })
    }

    /// Creates a new instance with the given migrations.
    ///
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use sqlx::{ SqlSafeStr, migrate::{Migration, MigrationType::*, Migrator}};
    ///
    /// // Define your migrations.
    /// // You can also use include_str!("./xxx.sql") instead of hard-coded SQL statements.
    /// let migrations = vec![
    ///     Migration::new(1, "user".into(), ReversibleUp, "create table users ( ... )".into_sql_str(), false),
    ///     Migration::new(2, "post".into(), ReversibleUp, "create table posts ( ... )".into_sql_str(), false),
    ///     // add more...
    ///  ];
    ///  let m = Migrator::with_migrations(migrations);
    /// ```
    pub fn with_migrations(mut migrations: Vec<Migration>) -> Self {
        migrations.sort();
        Self {
            migrations: Cow::Owned(migrations),
            ..Self::DEFAULT
        }
    }

    /// Override the name of the table used to track executed migrations.
    ///
    /// May be schema-qualified and/or contain quotes. Defaults to `_sqlx_migrations`.
    ///
    /// Potentially useful for multi-tenant databases.
    ///
    /// ### Warning: Potential Data Loss or Corruption!
    /// Changing this option for a production database will likely result in data loss or corruption
    /// as the migration machinery will no longer be aware of what migrations have been applied
    /// and will attempt to re-run them.
    ///
    /// You should create the new table as a copy of the existing migrations table (with contents!),
    /// and be sure all instances of your application have been migrated to the new
    /// table before deleting the old one.
    pub fn dangerous_set_table_name(&mut self, table_name: impl Into<Cow<'static, str>>) -> &Self {
        self.table_name = table_name.into();
        self
    }

    /// Add a schema name to be created if it does not already exist.
    ///
    /// May be used with [`Self::dangerous_set_table_name()`] to place the migrations table
    /// in a new schema without requiring it to exist first.
    ///
    /// ### Note: Support Depends on Database
    /// SQLite cannot create new schemas without attaching them to a database file,
    /// the path of which must be specified separately in an [`ATTACH DATABASE`](https://www.sqlite.org/lang_attach.html) command.
    pub fn create_schema(&mut self, schema_name: impl Into<Cow<'static, str>>) -> &Self {
        self.create_schemas.to_mut().push(schema_name.into());
        self
    }

    /// Specify whether applied migrations that are missing from the resolved migrations should be ignored.
    pub fn set_ignore_missing(&mut self, ignore_missing: bool) -> &mut Self {
        self.ignore_missing = ignore_missing;
        self
    }

    /// Specify whether or not to lock the database during migration. Defaults to `true`.
    ///
    /// ### Warning
    /// Disabling locking can lead to errors or data loss if multiple clients attempt to apply migrations simultaneously
    /// without some sort of mutual exclusion.
    ///
    /// This should only be used if the database does not support locking, e.g. CockroachDB which talks the Postgres
    /// protocol but does not support advisory locks used by SQLx's migrations support for Postgres.
    pub fn set_locking(&mut self, locking: bool) -> &mut Self {
        self.locking = locking;
        self
    }

    /// Get an iterator over all known migrations.
    pub fn iter(&self) -> slice::Iter<'_, Migration> {
        self.migrations.iter()
    }

    /// Check if a migration version exists.
    pub fn version_exists(&self, version: i64) -> bool {
        self.iter().any(|m| m.version == version)
    }

    /// Run any pending migrations against the database; and, validate previously applied migrations
    /// against the current migration source to detect accidental changes in previously-applied migrations.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use sqlx::migrate::MigrateError;
    /// # fn main() -> Result<(), MigrateError> {
    /// #     sqlx::__rt::test_block_on(async move {
    /// use sqlx::migrate::Migrator;
    /// use sqlx::sqlite::SqlitePoolOptions;
    ///
    /// let m = Migrator::new(std::path::Path::new("./migrations")).await?;
    /// let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
    /// m.run(&pool).await
    /// #     })
    /// # }
    /// ```
    pub async fn run<'a, A>(&self, migrator: A) -> Result<(), MigrateError>
    where
        A: Acquire<'a>,
        <A::Connection as Deref>::Target: Migrate,
    {
        let mut conn = migrator.acquire().await?;
        self.run_direct(None, &mut *conn).await
    }

    pub async fn run_to<'a, A>(&self, target: i64, migrator: A) -> Result<(), MigrateError>
    where
        A: Acquire<'a>,
        <A::Connection as Deref>::Target: Migrate,
    {
        let mut conn = migrator.acquire().await?;
        self.run_direct(Some(target), &mut *conn).await
    }

    // Getting around the annoying "implementation of `Acquire` is not general enough" error
    #[doc(hidden)]
    pub async fn run_direct<C>(&self, target: Option<i64>, conn: &mut C) -> Result<(), MigrateError>
    where
        C: Migrate,
    {
        // lock the database for exclusive access by the migrator
        if self.locking {
            conn.lock().await?;
        }

        for schema_name in self.create_schemas.iter() {
            conn.create_schema_if_not_exists(schema_name).await?;
        }

        // creates [_migrations] table only if needed
        // eventually this will likely migrate previous versions of the table
        conn.ensure_migrations_table(&self.table_name).await?;

        let version = conn.dirty_version(&self.table_name).await?;
        if let Some(version) = version {
            return Err(MigrateError::Dirty(version));
        }

        let applied_migrations = conn.list_applied_migrations(&self.table_name).await?;
        validate_applied_migrations(&applied_migrations, self)?;

        let applied_migrations: HashMap<_, _> = applied_migrations
            .into_iter()
            .map(|m| (m.version, m))
            .collect();

        for migration in self.iter() {
            if target.is_some_and(|target| target < migration.version) {
                // Target version reached
                break;
            }

            if migration.migration_type.is_down_migration() {
                continue;
            }

            match applied_migrations.get(&migration.version) {
                Some(applied_migration) => {
                    if migration.checksum != applied_migration.checksum {
                        return Err(MigrateError::VersionMismatch(migration.version));
                    }
                }
                None => {
                    conn.apply(&self.table_name, migration).await?;
                }
            }
        }

        // unlock the migrator to allow other migrators to run
        // but do nothing as we already migrated
        if self.locking {
            conn.unlock().await?;
        }

        Ok(())
    }

    /// Run down migrations against the database until a specific version.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use sqlx::migrate::MigrateError;
    /// # fn main() -> Result<(), MigrateError> {
    /// #     sqlx::__rt::test_block_on(async move {
    /// use sqlx::migrate::Migrator;
    /// use sqlx::sqlite::SqlitePoolOptions;
    ///
    /// let m = Migrator::new(std::path::Path::new("./migrations")).await?;
    /// let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
    /// m.undo(&pool, 4).await
    /// #     })
    /// # }
    /// ```
    pub async fn undo<'a, A>(&self, migrator: A, target: i64) -> Result<(), MigrateError>
    where
        A: Acquire<'a>,
        <A::Connection as Deref>::Target: Migrate,
    {
        let mut conn = migrator.acquire().await?;

        // lock the database for exclusive access by the migrator
        if self.locking {
            conn.lock().await?;
        }

        // creates [_migrations] table only if needed
        // eventually this will likely migrate previous versions of the table
        conn.ensure_migrations_table(&self.table_name).await?;

        let version = conn.dirty_version(&self.table_name).await?;
        if let Some(version) = version {
            return Err(MigrateError::Dirty(version));
        }

        let applied_migrations = conn.list_applied_migrations(&self.table_name).await?;
        validate_applied_migrations(&applied_migrations, self)?;

        let applied_migrations: HashMap<_, _> = applied_migrations
            .into_iter()
            .map(|m| (m.version, m))
            .collect();

        for migration in self
            .iter()
            .rev()
            .filter(|m| m.migration_type.is_down_migration())
            .filter(|m| applied_migrations.contains_key(&m.version))
            .filter(|m| m.version > target)
        {
            conn.revert(&self.table_name, migration).await?;
        }

        // unlock the migrator to allow other migrators to run
        // but do nothing as we already migrated
        if self.locking {
            conn.unlock().await?;
        }

        Ok(())
    }
}

fn validate_applied_migrations(
    applied_migrations: &[AppliedMigration],
    migrator: &Migrator,
) -> Result<(), MigrateError> {
    if migrator.ignore_missing {
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
