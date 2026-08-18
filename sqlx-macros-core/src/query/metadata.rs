use sqlx_core::config::Config;
use std::hash::{BuildHasherDefault, DefaultHasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::query::cache::{MtimeCache, MtimeCacheBuilder};
use sqlx_core::HashMap;

pub struct Metadata {
    pub manifest_dir: PathBuf,
    pub config: Config,
    env: MtimeCache<Arc<MacrosEnv>>,
    workspace_root: Arc<Mutex<Option<PathBuf>>>,
}

#[cfg_attr(test, derive(Debug))]
pub struct MacrosEnv {
    pub database_url: Option<String>,
    pub offline_dir: Option<PathBuf>,
    pub offline: Option<bool>,
}

#[derive(thiserror::Error, Debug)]
#[error("error reading dotenv file {path:?}")]
struct DotenvError {
    path: PathBuf,
    #[source]
    error: dotenvy::Error,
}

impl Metadata {
    pub fn env(&self) -> crate::Result<Arc<MacrosEnv>> {
        let workspace_root = self.workspace_root();

        self.env.get_or_try_init(|builder| {
            load_env(&self.manifest_dir, &workspace_root, &self.config, builder)
        })
    }

    pub fn workspace_root(&self) -> PathBuf {
        let mut root = self.workspace_root.lock().unwrap();
        if root.is_none() {
            use serde::Deserialize;
            use std::process::Command;

            let cargo = crate::env("CARGO").unwrap();

            let output = Command::new(cargo)
                .args(["metadata", "--format-version=1", "--no-deps"])
                .current_dir(&self.manifest_dir)
                .env_remove("__CARGO_FIX_PLZ")
                .output()
                .expect("Could not fetch metadata");

            #[derive(Deserialize)]
            struct CargoMetadata {
                workspace_root: PathBuf,
            }

            let metadata: CargoMetadata =
                serde_json::from_slice(&output.stdout).expect("Invalid `cargo metadata` output");

            *root = Some(metadata.workspace_root);
        }
        root.clone().unwrap()
    }
}

pub fn try_for_crate() -> crate::Result<Arc<Metadata>> {
    /// The `MtimeCache` in this type covers the config itself,
    /// any changes to which will indirectly invalidate the loaded env vars as well.
    #[expect(clippy::type_complexity)]
    static METADATA: Mutex<
        HashMap<String, Arc<MtimeCache<Arc<Metadata>>>, BuildHasherDefault<DefaultHasher>>,
    > = Mutex::new(HashMap::with_hasher(BuildHasherDefault::new()));

    let manifest_dir = crate::env("CARGO_MANIFEST_DIR")?;

    let cache = METADATA
        .lock()
        .expect("BUG: we shouldn't panic while holding this lock")
        .entry_ref(&manifest_dir)
        .or_insert_with(|| Arc::new(MtimeCache::new()))
        .clone();

    cache.get_or_try_init(|builder| {
        let manifest_dir = PathBuf::from(manifest_dir);
        let config_path = manifest_dir.join("sqlx.toml");

        builder.add_path(config_path.clone());

        let config = Config::try_from_path_or_default(config_path)?;

        Ok(Arc::new(Metadata {
            manifest_dir,
            config,
            env: MtimeCache::new(),
            workspace_root: Default::default(),
        }))
    })
}

fn load_env(
    manifest_dir: &Path,
    workspace_root: &Path,
    config: &Config,
    builder: &mut MtimeCacheBuilder,
) -> crate::Result<Arc<MacrosEnv>> {
    let from_env = MacrosEnv {
        database_url: crate::env_opt(config.common.database_url_var())?,
        offline_dir: crate::env_opt("SQLX_OFFLINE_DIR")?.map(PathBuf::from),
        offline: crate::env_opt("SQLX_OFFLINE")?.map(|val| is_truthy_bool(&val)),
    };

    load_env_from_sources(manifest_dir, workspace_root, config, builder, from_env)
}

fn load_env_from_sources(
    manifest_dir: &Path,
    workspace_root: &Path,
    config: &Config,
    builder: &mut MtimeCacheBuilder,
    from_env: MacrosEnv,
) -> crate::Result<Arc<MacrosEnv>> {
    let mut from_dotenv = MacrosEnv {
        database_url: None,
        offline_dir: None,
        offline: None,
    };

    // https://github.com/launchbadge/sqlx/issues/4276
    let dirs = if manifest_dir.starts_with(workspace_root) {
        // Often just `[manifest_dir, workspace_dir]` but project structures can absolutely
        // be more complicated
        manifest_dir
            .ancestors()
            .take_while(|dir| dir.starts_with(workspace_root))
            .collect::<Vec<_>>()
    } else {
        // Thinking of edge cases, there's the possibility that the package directory
        // isn't actually a child of the workspace directory. There isn't really any other sane
        // thing to do here; we shouldn't traverse into unrelated paths.
        [manifest_dir, workspace_root].to_vec()
    };

    for dir in dirs {
        let path = dir.join(".env");

        let dotenv = match dotenvy::from_path_iter(&path) {
            Ok(iter) => {
                builder.add_path(path.clone());
                iter
            }
            Err(dotenvy::Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                builder.add_path(dir.to_path_buf());
                continue;
            }
            Err(dotenvy::Error::Io(_))
                if has_query_source_without_dotenv(&from_env, &from_dotenv) =>
            {
                builder.add_path(path.clone());
                continue;
            }
            Err(e) => {
                builder.add_path(path.clone());
                return Err(DotenvError { path, error: e }.into());
            }
        };

        read_dotenv(&path, dotenv, config, &from_env, &mut from_dotenv)?;
    }

    Ok(Arc::new(MacrosEnv {
        // Make set variables take precedent
        database_url: from_env.database_url.or(from_dotenv.database_url),
        offline_dir: from_env.offline_dir.or(from_dotenv.offline_dir),
        offline: from_env.offline.or(from_dotenv.offline),
    }))
}

fn read_dotenv(
    path: &Path,
    dotenv: impl Iterator<Item = dotenvy::Result<(String, String)>>,
    config: &Config,
    from_env: &MacrosEnv,
    from_dotenv: &mut MacrosEnv,
) -> crate::Result<()> {
    let ignore_io_error = has_query_source_without_dotenv(from_env, from_dotenv);

    for res in dotenv {
        let (name, val) = match res {
            Ok(pair) => pair,
            Err(dotenvy::Error::Io(_)) if ignore_io_error => {
                break;
            }
            Err(error) => {
                return Err(DotenvError {
                    path: path.to_path_buf(),
                    error,
                }
                .into());
            }
        };

        match &*name {
            "SQLX_OFFLINE_DIR" => from_dotenv.offline_dir = Some(val.into()),
            "SQLX_OFFLINE" => from_dotenv.offline = Some(is_truthy_bool(&val)),
            _ if name == config.common.database_url_var() => from_dotenv.database_url = Some(val),
            _ => continue,
        }
    }

    Ok(())
}

fn has_query_source_without_dotenv(from_env: &MacrosEnv, from_dotenv: &MacrosEnv) -> bool {
    let offline = from_env.offline.or(from_dotenv.offline);
    let database_url = from_env
        .database_url
        .as_deref()
        .or(from_dotenv.database_url.as_deref());

    offline == Some(true) || database_url.is_some_and(|url| !url.is_empty())
}

/// Returns `true` if `val` is `"true"`,
fn is_truthy_bool(val: &str) -> bool {
    val.eq_ignore_ascii_case("true") || val == "1"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_ID: AtomicU64 = AtomicU64::new(0);

    struct ReadThenError(Option<&'static [u8]>);

    impl Read for ReadThenError {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(bytes) = self.0.take() else {
                return Err(io::Error::other("test read failure"));
            };

            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = TEMP_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sqlx-macros-core-env-test-{}-{id}",
                std::process::id()
            ));

            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn load_test_env(
        manifest_dir: &Path,
        workspace_root: &Path,
        from_env: MacrosEnv,
    ) -> crate::Result<Arc<MacrosEnv>> {
        MtimeCache::new().get_or_try_init(|builder| {
            load_env_from_sources(
                manifest_dir,
                workspace_root,
                &Config::default(),
                builder,
                from_env,
            )
        })
    }

    fn empty_env() -> MacrosEnv {
        MacrosEnv {
            database_url: None,
            offline_dir: None,
            offline: None,
        }
    }

    #[test]
    fn database_url_allows_unreadable_parent_dotenv() {
        let workspace = TestDir::new();
        let manifest_dir = workspace.path().join("crate");
        fs::create_dir(&manifest_dir).unwrap();
        fs::create_dir(workspace.path().join(".env")).unwrap();

        let env = load_test_env(
            &manifest_dir,
            workspace.path(),
            MacrosEnv {
                database_url: Some("postgres://from-environment".into()),
                ..empty_env()
            },
        )
        .unwrap();

        assert_eq!(
            env.database_url.as_deref(),
            Some("postgres://from-environment")
        );
        assert_eq!(env.offline, None);
    }

    #[test]
    fn process_environment_takes_precedence_over_dotenv() {
        let workspace = TestDir::new();
        let manifest_dir = workspace.path().join("crate");
        fs::create_dir(&manifest_dir).unwrap();
        fs::write(
            manifest_dir.join(".env"),
            "DATABASE_URL=postgres://from-dotenv\nSQLX_OFFLINE=true\n",
        )
        .unwrap();

        let env = load_test_env(
            &manifest_dir,
            workspace.path(),
            MacrosEnv {
                database_url: Some("postgres://from-environment".into()),
                offline_dir: None,
                offline: Some(false),
            },
        )
        .unwrap();

        assert_eq!(
            env.database_url.as_deref(),
            Some("postgres://from-environment")
        );
        assert_eq!(env.offline, Some(false));
    }

    #[test]
    fn dotenv_offline_is_used_with_database_url_from_environment() {
        let workspace = TestDir::new();
        let manifest_dir = workspace.path().join("crate");
        fs::create_dir(&manifest_dir).unwrap();
        fs::write(manifest_dir.join(".env"), "SQLX_OFFLINE=true\n").unwrap();

        let env = load_test_env(
            &manifest_dir,
            workspace.path(),
            MacrosEnv {
                database_url: Some("postgres://from-environment".into()),
                ..empty_env()
            },
        )
        .unwrap();

        assert_eq!(
            env.database_url.as_deref(),
            Some("postgres://from-environment")
        );
        assert_eq!(env.offline, Some(true));
    }

    #[test]
    fn offline_environment_allows_unreadable_parent_dotenv() {
        let workspace = TestDir::new();
        let manifest_dir = workspace.path().join("crate");
        fs::create_dir(&manifest_dir).unwrap();
        fs::create_dir(workspace.path().join(".env")).unwrap();

        let env = load_test_env(
            &manifest_dir,
            workspace.path(),
            MacrosEnv {
                offline: Some(true),
                ..empty_env()
            },
        )
        .unwrap();

        assert_eq!(env.database_url, None);
        assert_eq!(env.offline, Some(true));
    }

    #[test]
    fn local_database_url_allows_unreadable_parent_dotenv() {
        let workspace = TestDir::new();
        let manifest_dir = workspace.path().join("crate");
        fs::create_dir(&manifest_dir).unwrap();
        fs::write(
            manifest_dir.join(".env"),
            "DATABASE_URL=postgres://from-dotenv\n",
        )
        .unwrap();
        fs::create_dir(workspace.path().join(".env")).unwrap();

        let env = load_test_env(&manifest_dir, workspace.path(), empty_env()).unwrap();

        assert_eq!(env.database_url.as_deref(), Some("postgres://from-dotenv"));
        assert_eq!(env.offline, None);
    }

    #[test]
    fn missing_environment_does_not_hide_dotenv_io_error() {
        let workspace = TestDir::new();
        let manifest_dir = workspace.path().join("crate");
        fs::create_dir(&manifest_dir).unwrap();
        fs::create_dir(workspace.path().join(".env")).unwrap();

        let error = load_test_env(&manifest_dir, workspace.path(), empty_env())
            .expect_err("loading an unusable dotenv should fail without environment values");

        assert!(error.to_string().contains("error reading dotenv file"));
    }

    #[test]
    fn malformed_dotenv_is_not_hidden_by_database_url() {
        let workspace = TestDir::new();
        let manifest_dir = workspace.path().join("crate");
        fs::create_dir(&manifest_dir).unwrap();
        fs::write(manifest_dir.join(".env"), "INVALID LINE\n").unwrap();

        let error = load_test_env(
            &manifest_dir,
            workspace.path(),
            MacrosEnv {
                database_url: Some("postgres://from-environment".into()),
                ..empty_env()
            },
        )
        .expect_err("a malformed dotenv should still be reported");

        assert!(error.to_string().contains("error reading dotenv file"));
    }

    #[test]
    fn dotenv_io_error_after_database_url_is_not_hidden() {
        let from_env = empty_env();
        let mut from_dotenv = empty_env();
        let error = read_dotenv(
            Path::new(".env"),
            dotenvy::from_read_iter(ReadThenError(Some(b"DATABASE_URL=sqlite://test.db\n"))),
            &Config::default(),
            &from_env,
            &mut from_dotenv,
        )
        .expect_err("an I/O error in a partially read dotenv should be reported");

        assert_eq!(
            from_dotenv.database_url.as_deref(),
            Some("sqlite://test.db")
        );
        assert!(error.to_string().contains("error reading dotenv file"));
    }

    #[test]
    fn existing_query_source_allows_later_dotenv_io_error() {
        let from_env = MacrosEnv {
            database_url: Some("sqlite://test.db".into()),
            ..empty_env()
        };
        let mut from_dotenv = empty_env();

        read_dotenv(
            Path::new(".env"),
            dotenvy::from_read_iter(ReadThenError(None)),
            &Config::default(),
            &from_env,
            &mut from_dotenv,
        )
        .expect("a later dotenv I/O error should not hide an existing query source");
    }
}
