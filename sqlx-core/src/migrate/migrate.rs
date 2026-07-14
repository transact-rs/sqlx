use crate::error::Error;
use crate::migrate::{AppliedMigration, MigrateError, Migration};
use futures_core::future::BoxFuture;
use std::future::Future;
use std::time::Duration;

pub trait MigrateDatabase {
    // create database in url
    // uses a maintenance database depending on driver
    fn create_database(url: &str) -> impl Future<Output = Result<(), Error>> + Send + '_;

    // check if the database in url exists
    // uses a maintenance database depending on driver
    fn database_exists(url: &str) -> impl Future<Output = Result<bool, Error>> + Send + '_;

    // drop database in url
    // uses a maintenance database depending on driver
    fn drop_database(url: &str) -> impl Future<Output = Result<(), Error>> + Send + '_;

    // force drop database in url
    // uses a maintenance database depending on driver
    fn force_drop_database(_url: &str) -> impl Future<Output = Result<(), Error>> + Send + '_ {
        async { Err(MigrateError::ForceNotSupported)? }
    }
}

// 'e = Executor
pub trait Migrate {
    /// Create a database schema with the given name if it does not already exist.
    fn create_schema_if_not_exists<'e>(
        &'e mut self,
        schema_name: &'e str,
    ) -> BoxFuture<'e, Result<(), MigrateError>>;

    // ensure migrations table exists
    // will create or migrate it if needed
    fn ensure_migrations_table<'e>(
        &'e mut self,
        table_name: &'e str,
    ) -> BoxFuture<'e, Result<(), MigrateError>>;

    // Return the version on which the database is dirty or None otherwise.
    // "dirty" means there is a partially applied migration that failed.
    fn dirty_version<'e>(
        &'e mut self,
        table_name: &'e str,
    ) -> BoxFuture<'e, Result<Option<i64>, MigrateError>>;

    // Return the ordered list of applied migrations
    fn list_applied_migrations<'e>(
        &'e mut self,
        table_name: &'e str,
    ) -> BoxFuture<'e, Result<Vec<AppliedMigration>, MigrateError>>;

    // Should acquire a database lock so that only one migration process
    // can run at a time. [`Migrate`] will call this function before applying
    // any migrations.
    fn lock(&mut self) -> BoxFuture<'_, Result<(), MigrateError>>;

    // Should release the lock. [`Migrate`] will call this function after all
    // migrations have been run.
    fn unlock(&mut self) -> BoxFuture<'_, Result<(), MigrateError>>;

    // run SQL from migration in a DDL transaction
    // insert new row to [_migrations] table on completion (success or failure)
    // returns the time taking to run the migration SQL
    fn apply<'e>(
        &'e mut self,
        table_name: &'e str,
        migration: &'e Migration,
    ) -> BoxFuture<'e, Result<Duration, MigrateError>>;

    // run a revert SQL from migration in a DDL transaction
    // deletes the row in [_migrations] table with specified migration version on completion (success or failure)
    // returns the time taking to run the migration SQL
    fn revert<'e>(
        &'e mut self,
        table_name: &'e str,
        migration: &'e Migration,
    ) -> BoxFuture<'e, Result<Duration, MigrateError>>;

    // insert new row to [_migrations] table without running SQL from migration
    fn skip<'e>(
        &'e mut self,
        _table_name: &'e str,
        _migration: &'e Migration,
    ) -> BoxFuture<'e, Result<(), MigrateError>> {
        Box::pin(async move { Err(MigrateError::SkipNotSupported()) })
    }
}
