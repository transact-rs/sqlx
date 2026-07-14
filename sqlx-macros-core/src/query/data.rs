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

        // From a quick survey of the files generated by `examples/postgres/axum-social-with-tests`,
        // which are generally in the 1-2 KiB range, this seems like a safe bet to avoid
        // lots of reallocations without using too much memory.
        //
        // As of writing, `serde_json::to_vec_pretty()` only allocates 128 bytes up-front.
        let mut data = Vec::with_capacity(4096);

        serde_json::to_writer_pretty(&mut data, self).expect("BUG: failed to serialize query data");

        // Ensure there is a newline at the end of the JSON file to avoid
        // accidental modification by IDE and make github diff tool happier.
        data.push(b'\n');

        // This ideally writes the data in as few syscalls as possible.
        file.write_all(&data)
            .map_err(|err| format!("failed to write query data to file {path:?}: {err:?}"))?;

        // We don't really need to call `.sync_data()` since it's trivial to re-run the macro
        // in the event a power loss results in incomplete flushing of the data to disk.

        Ok(())
    }
}

pub(super) fn hash_string(query: &str) -> String {
    // picked `sha2` because it's already in the dependency tree for both MySQL and Postgres
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(query.as_bytes()))
}
