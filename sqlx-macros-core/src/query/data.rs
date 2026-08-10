use std::fmt::{Debug, Display, Formatter};
use std::fs;
use std::io::Write as _;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Serialize, Serializer};

use sqlx_core::database::Database;
use sqlx_core::describe::Describe;
use sqlx_core::HashMap;

use crate::database::DatabaseExt;
use crate::query::cache::MtimeCache;

#[derive(serde::Serialize)]
#[serde(bound(serialize = "Describe<DB>: serde::Serialize"))]
#[derive(Debug)]
pub struct QueryData<DB: Database> {
    db_name: SerializeDbName<DB>,
    #[allow(dead_code)]
    pub(super) query: String,
    pub(super) describe: Describe<DB>,
    pub(super) hash: String,
}

impl<DB: Database> QueryData<DB> {
    pub fn from_describe(query: &str, describe: Describe<DB>) -> Self {
        QueryData {
            db_name: SerializeDbName::default(),
            query: query.into(),
            describe,
            hash: hash_string(query),
        }
    }
}

struct SerializeDbName<DB>(PhantomData<DB>);

impl<DB> Default for SerializeDbName<DB> {
    fn default() -> Self {
        SerializeDbName(PhantomData)
    }
}

impl<DB: Database> Debug for SerializeDbName<DB> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SerializeDbName").field(&DB::NAME).finish()
    }
}

impl<DB: Database> Display for SerializeDbName<DB> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.pad(DB::NAME)
    }
}

impl<DB: Database> Serialize for SerializeDbName<DB> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(DB::NAME)
    }
}

static OFFLINE_DATA_CACHE: LazyLock<Mutex<HashMap<PathBuf, Arc<MtimeCache<DynQueryData>>>>> =
    LazyLock::new(Default::default);

/// Offline query data
#[derive(Clone, serde::Deserialize)]
pub struct DynQueryData {
    pub db_name: String,
    pub query: String,
    pub describe: serde_json::Value,
    pub hash: String,
}

impl DynQueryData {
    /// Loads a query given the path to its "query-<hash>.json" file. Subsequent calls for the same
    /// path are retrieved from an in-memory cache.
    pub fn from_data_file(path: &Path, query: &str) -> crate::Result<Self> {
        let cache = OFFLINE_DATA_CACHE
            .lock()
            // Just reset the cache on error
            .unwrap_or_else(|poison_err| {
                let mut guard = poison_err.into_inner();
                *guard = Default::default();
                guard
            })
            .entry_ref(path)
            .or_insert_with(|| Arc::new(MtimeCache::new()))
            .clone();

        cache.get_or_try_init(|builder| {
            builder.add_path(path.into());

            let offline_data_contents = fs::read_to_string(path).map_err(|e| {
                format!("failed to read saved query path {}: {}", path.display(), e)
            })?;
            let dyn_data: DynQueryData = serde_json::from_str(&offline_data_contents)?;

            if query != dyn_data.query {
                return Err("hash collision for saved query data".into());
            }

            Ok(dyn_data)
        })
    }
}

impl<DB: DatabaseExt> QueryData<DB>
where
    Describe<DB>: serde::Serialize + serde::de::DeserializeOwned,
{
    pub fn from_dyn_data(dyn_data: DynQueryData) -> crate::Result<Self> {
        assert!(!dyn_data.db_name.is_empty());
        assert!(!dyn_data.hash.is_empty());

        if DB::NAME == dyn_data.db_name {
            let describe: Describe<DB> = serde_json::from_value(dyn_data.describe)?;
            Ok(QueryData {
                db_name: SerializeDbName::default(),
                query: dyn_data.query,
                describe,
                hash: dyn_data.hash,
            })
        } else {
            Err(format!(
                "expected query data for {}, got data for {}",
                DB::NAME,
                dyn_data.db_name
            )
            .into())
        }
    }

    pub(super) fn save_in(&self, dir: &Path) -> crate::Result<()> {
        use std::io::ErrorKind;

        let path = dir.join(format!("query-{}.json", self.hash));

        if let Err(err) = fs::remove_file(&path) {
            match err.kind() {
                ErrorKind::NotFound | ErrorKind::PermissionDenied => (),
                ErrorKind::NotADirectory => {
                    return Err(format!(
                        "sqlx offline path exists, but is not a directory: {dir:?}"
                    )
                    .into());
                }
                _ => return Err(format!("failed to delete {path:?}: {err:?}").into()),
            }
        }

        // Prevent tearing from concurrent invocations possibly trying to write the same file
        // by using the existence of the file itself as a mutex.
        //
        // By deleting the file first and then using `.create_new(true)`,
        // we guarantee that this only succeeds if another invocation hasn't concurrently
        // re-created the file.
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) => {
                return match err.kind() {
                    // We overlapped with a concurrent invocation and the other one succeeded.
                    ErrorKind::AlreadyExists => Ok(()),
                    ErrorKind::NotFound => {
                        Err(format!("sqlx offline path does not exist: {dir:?}").into())
                    }
                    ErrorKind::NotADirectory => Err(format!(
                        "sqlx offline path exists, but is not a directory: {dir:?}"
                    )
                    .into()),
                    _ => Err(format!("failed to exclusively create {path:?}: {err:?}").into()),
                };
            }
        };

        let data = serialize_query_data(self);

        // This ideally writes the data in as few syscalls as possible.
        file.write_all(&data)
            .map_err(|err| format!("failed to write query data to file {path:?}: {err:?}"))?;

        // We don't really need to call `.sync_data()` since it's trivial to re-run the macro
        // in the event a power loss results in incomplete flushing of the data to disk.

        Ok(())
    }
}

fn serialize_query_data(data: &impl Serialize) -> Vec<u8> {
    // From a quick survey of the files generated by `examples/postgres/axum-social-with-tests`,
    // which are generally in the 1-2 KiB range, this seems like a safe bet to avoid
    // lots of reallocations without using too much memory.
    //
    // As of writing, `serde_json::to_vec_pretty()` only allocates 128 bytes up-front.
    let mut serialized = Vec::with_capacity(4096);
    let mut data =
        serde_json::to_value(data).expect("BUG: failed to convert query data to JSON value");

    data.sort_all_objects();

    serde_json::to_writer_pretty(&mut serialized, &data)
        .expect("BUG: failed to serialize query data");

    // Ensure there is a newline at the end of the JSON file to avoid
    // accidental modification by IDE and make github diff tool happier.
    serialized.push(b'\n');
    serialized
}

pub(super) fn hash_string(query: &str) -> String {
    // picked `sha2` because it's already in the dependency tree for both MySQL and Postgres
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(query.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::serialize_query_data;
    use serde::ser::SerializeMap;
    use serde::{Serialize, Serializer};

    enum OrderedValue<'a> {
        Number(u64),
        Object(Vec<(&'a str, OrderedValue<'a>)>),
    }

    impl Serialize for OrderedValue<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match self {
                Self::Number(number) => serializer.serialize_u64(*number),
                Self::Object(entries) => {
                    let mut map = serializer.serialize_map(Some(entries.len()))?;
                    for (key, value) in entries {
                        map.serialize_entry(key, value)?;
                    }
                    map.end()
                }
            }
        }
    }

    #[test]
    fn query_data_json_keys_are_sorted_recursively() {
        let first = OrderedValue::Object(vec![
            (
                "z",
                OrderedValue::Object(vec![
                    ("second", OrderedValue::Number(2)),
                    ("first", OrderedValue::Number(1)),
                ]),
            ),
            ("a", OrderedValue::Number(0)),
        ]);
        let second = OrderedValue::Object(vec![
            ("a", OrderedValue::Number(0)),
            (
                "z",
                OrderedValue::Object(vec![
                    ("first", OrderedValue::Number(1)),
                    ("second", OrderedValue::Number(2)),
                ]),
            ),
        ]);

        let expected =
            b"{\n  \"a\": 0,\n  \"z\": {\n    \"first\": 1,\n    \"second\": 2\n  }\n}\n";
        let first = serialize_query_data(&first);
        let second = serialize_query_data(&second);

        assert_eq!(first, expected);
        assert_eq!(second, expected);
        assert_eq!(first, second);
    }

    #[cfg(feature = "_sqlite")]
    #[test]
    fn sqlite_query_data_is_stable_across_repeated_saves() {
        use super::{DynQueryData, QueryData};
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        struct RemoveOnDrop(PathBuf);

        impl Drop for RemoveOnDrop {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }

        static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sqlite-query.json"
        ));
        let dyn_data: DynQueryData = serde_json::from_str(fixture).unwrap();
        let query_data = QueryData::<sqlx_sqlite::Sqlite>::from_dyn_data(dyn_data).unwrap();
        let test_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/sqlx-macros-core-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&test_dir).unwrap();
        let _remove_on_drop = RemoveOnDrop(test_dir.clone());
        let query_path = test_dir.join(format!("query-{}.json", query_data.hash));

        query_data.save_in(&test_dir).unwrap();
        let first = std::fs::read(&query_path).unwrap();
        query_data.save_in(&test_dir).unwrap();
        let second = std::fs::read(query_path).unwrap();

        assert_eq!(first, fixture.as_bytes());
        assert_eq!(second, fixture.as_bytes());
        assert_eq!(first, second);
    }
}
