use crate::error::Error;
use crate::pool::PoolOptions;
use crate::testing::{FixtureSnapshot, TestArgs, TestContext, TestSupport};
use crate::{Sqlite, SqliteConnectOptions};
use sqlx_core::config::Config;
use std::future::Future;
use std::path::{Path, PathBuf};

pub(crate) use sqlx_core::testing::*;

const BASE_PATH: &str = "target/sqlx/test-dbs";

impl TestSupport for Sqlite {
    fn test_context(
        args: &TestArgs,
    ) -> impl Future<Output = Result<TestContext<Self>, Error>> + Send + '_ {
        test_context(args)
    }

    async fn cleanup_test(db_name: &str) -> Result<(), Error> {
        crate::fs::remove_file(db_name).await?;
        Ok(())
    }

    async fn cleanup_test_dbs() -> Result<Option<usize>, Error> {
        crate::fs::remove_dir_all(BASE_PATH).await?;
        Ok(None)
    }

    async fn snapshot(_conn: &mut Self::Connection) -> Result<FixtureSnapshot<Self>, Error> {
        todo!()
    }

    fn db_name(args: &TestArgs) -> String {
        convert_path(args.test_path)
    }
}

async fn test_context(args: &TestArgs) -> Result<TestContext<Sqlite>, Error> {
    let db_path = convert_path(args.test_path);

    if let Some(parent_path) = Path::parent(db_path.as_ref()) {
        crate::fs::create_dir_all(parent_path)
            .await
            .expect("failed to create folders");
    }

    if Path::exists(db_path.as_ref()) {
        crate::fs::remove_file(&db_path)
            .await
            .expect("failed to remove database from previous test run");
    }

    let connect_opts = apply_sqlx_toml_config(
        SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true),
    )?;

    Ok(TestContext {
        connect_opts,
        // This doesn't really matter for SQLite as the databases are independent of each other.
        // The main limitation is going to be the number of concurrent running tests.
        pool_opts: PoolOptions::new().max_connections(1000),
        db_name: db_path,
    })
}

/// Apply `drivers.sqlite` config from `sqlx.toml` (e.g. `unsafe-load-extensions`)
/// so that `#[sqlx::test]` databases behave the same as connections made via
/// `sqlx::query!()` or `sqlx-cli`, which already read this configuration.
fn apply_sqlx_toml_config(opts: SqliteConnectOptions) -> Result<SqliteConnectOptions, Error> {
    let config = Config::try_from_crate_or_default().map_err(Error::config)?;
    opts.apply_driver_config(&config.drivers.sqlite)
}

fn convert_path(test_path: &str) -> String {
    let mut path = PathBuf::from(BASE_PATH);

    for segment in test_path.split("::") {
        path.push(segment);
    }

    path.set_extension("sqlite");

    path.into_os_string()
        .into_string()
        .expect("path should be UTF-8")
}

#[test]
fn test_convert_path() {
    let path = convert_path("foo::bar::baz::quux");

    assert_eq!(path, "target/sqlx/test-dbs/foo/bar/baz/quux.sqlite");
}

// Regression test for https://github.com/launchbadge/sqlx/issues/4372:
// `test_context()` built `SqliteConnectOptions` directly and never applied
// `drivers.sqlite` from `sqlx.toml`, unlike `sqlx::query!()` and `sqlx-cli`
// (see `SqliteConnectOptions::apply_driver_config`).
//
// `sqlx-sqlite/sqlx.toml` (this crate's own, used only by this test) sets
// `unsafe-load-extensions` to a marker name. Before the fix, this config was
// never read at all; after the fix, `apply_sqlx_toml_config()` (used by
// `test_context()`) applies it to the options `#[sqlx::test]` connects with.
#[cfg(feature = "load-extension")]
#[test]
fn test_context_applies_sqlx_toml_driver_config() {
    let opts = apply_sqlx_toml_config(SqliteConnectOptions::new())
        .expect("applying sqlx-sqlite/sqlx.toml's `unsafe-load-extensions` should succeed regardless of whether the named extension actually exists, since SQLite only loads it lazily on connect");

    assert!(
        opts.extensions
            .contains_key("sqlx-issue-4372-regression-test-marker"),
        "expected `unsafe-load-extensions` from sqlx-sqlite/sqlx.toml to be applied, got: {:?}",
        opts.extensions
    );
}
