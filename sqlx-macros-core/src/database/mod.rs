use sqlx_core::config;
use sqlx_core::connection::Connection;
use sqlx_core::database::Database;
use sqlx_core::describe::Describe;
use sqlx_core::executor::Executor;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_core::sql_str::SqlSafeStr;
use sqlx_core::type_checking::TypeChecking;
use std::collections::hash_map;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

#[cfg(any(feature = "postgres", feature = "mysql", feature = "_sqlite"))]
mod impls;

pub trait DatabaseExt: Database + TypeChecking {
    const DATABASE_PATH: &'static str;
    const ROW_PATH: &'static str;

    fn db_path() -> syn::Path {
        syn::parse_str(Self::DATABASE_PATH).unwrap()
    }

    fn row_path() -> syn::Path {
        syn::parse_str(Self::ROW_PATH).unwrap()
    }

    fn describe_blocking(
        query: &str,
        database_url: &str,
        driver_config: &config::drivers::Config,
    ) -> sqlx_core::Result<Describe<Self>>;

    /// Prepare a freshly-opened connection used by the query macros for `describe`.
    ///
    /// Defaults to a no-op. Postgres overrides this to force a generic query plan,
    /// which gives more accurate nullability inference for parameterized queries
    /// (see launchbadge/sqlx#3541). The override is gated so it is skipped where it
    /// doesn't apply -- e.g. CockroachDB, which rejected the previous implementation
    /// (see launchbadge/sqlx#4274).
    #[allow(async_fn_in_trait)]
    async fn prepare_describe_connection(_conn: &mut Self::Connection) -> sqlx_core::Result<()> {
        Ok(())
    }
}

#[allow(dead_code)]
pub struct CachingDescribeBlocking<DB: DatabaseExt> {
    connections: LazyLock<Mutex<HashMap<String, DB::Connection>>>,
}

#[allow(dead_code)]
impl<DB: DatabaseExt> CachingDescribeBlocking<DB> {
    #[allow(clippy::new_without_default, reason = "internal API")]
    pub const fn new() -> Self {
        CachingDescribeBlocking {
            connections: LazyLock::new(|| Mutex::new(HashMap::new())),
        }
    }

    pub fn describe(
        &self,
        query: &str,
        database_url: &str,
        driver_config: &config::drivers::Config,
    ) -> sqlx_core::Result<Describe<DB>>
    where
        for<'a> &'a mut DB::Connection: Executor<'a, Database = DB>,
    {
        let mut cache = self
            .connections
            .lock()
            .expect("previous panic in describe call");

        crate::block_on(async {
            let conn = match cache.entry(database_url.to_string()) {
                hash_map::Entry::Occupied(hit) => hit.into_mut(),
                hash_map::Entry::Vacant(miss) => {
                    let conn = miss.insert(
                        DB::Connection::connect_with_driver_config(database_url, driver_config)
                            .await?,
                    );
                    DB::prepare_describe_connection(conn).await?;
                    conn
                }
            };

            match conn
                .describe(AssertSqlSafe(query.to_string()).into_sql_str())
                .await
            {
                Ok(describe) => Ok(describe),
                Err(e) => {
                    if matches!(e, sqlx_core::Error::Io(_) | sqlx_core::Error::Protocol(_)) {
                        cache.remove(database_url);
                    }

                    Err(e)
                }
            }
        })
    }
}
