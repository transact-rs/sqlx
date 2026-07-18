use crate::error::Error;
use crate::pool::PoolOptions;
use crate::testing::{FixtureSnapshot, TestArgs, TestContext, TestSupport};
use crate::{Sqlite, SqliteConnectOptions};
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

    async fn cleanup_test(args: &TestArgs) -> Result<(), Error> {
        let db_name = Self::db_name(args);
        crate::fs::remove_file(&db_name).await?;
        Ok(())
    }

    async fn cleanup_test_dbs() -> Result<Option<usize>, Error> {
        crate::fs::remove_dir_all(BASE_PATH).await?;
        Ok(None)
    }

    async fn cleanup_test_dbs_by_url(_: &str) -> Result<Option<usize>, Error> {
        unimplemented!()
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

    Ok(TestContext {
        connect_opts: SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true),
        // This doesn't really matter for SQLite as the databases are independent of each other.
        // The main limitation is going to be the number of concurrent running tests.
        pool_opts: PoolOptions::new().max_connections(1000),
        db_name: db_path,
    })
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
