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

pub struct MacrosEnv {
    pub database_url: Option<String>,
    pub offline_dir: Option<PathBuf>,
    pub offline: Option<bool>,
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
            *root = Some(resolve_workspace_root(
                crate::env("SQLX_WORKSPACE_DIR").ok().map(PathBuf::from),
                || {
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

                    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)
                        .expect("Invalid `cargo metadata` output");

                    metadata.workspace_root
                },
            ));
        }
        root.clone().unwrap()
    }
}

fn resolve_workspace_root(
    override_dir: Option<PathBuf>,
    cargo_fallback: impl FnOnce() -> PathBuf,
) -> PathBuf {
    override_dir.unwrap_or_else(cargo_fallback)
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

fn load_from_dotenv(
    manifest_dir: &Path,
    workspace_root: &Path,
    config: &Config,
    builder: &mut MtimeCacheBuilder,
) -> crate::Result<Arc<MacrosEnv>> {
    #[derive(thiserror::Error, Debug)]
    #[error("error reading dotenv file {path:?}")]
    struct DotenvError {
        path: PathBuf,
        #[source]
        error: dotenvy::Error,
    }

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
            Err(e) => {
                builder.add_path(path.clone());
                return Err(DotenvError { path, error: e }.into());
            }
        };

        for res in dotenv {
            let (name, val) = res.map_err(|e| DotenvError {
                path: path.clone(),
                error: e,
            })?;

            match &*name {
                "SQLX_OFFLINE_DIR" => from_dotenv.offline_dir = Some(val.into()),
                "SQLX_OFFLINE" => from_dotenv.offline = Some(is_truthy_bool(&val)),
                _ if name == config.common.database_url_var() => {
                    from_dotenv.database_url = Some(val)
                }
                _ => continue,
            }
        }
    }

    Ok(Arc::new(from_dotenv))
}

fn load_env(
    manifest_dir: &Path,
    config: &Config,
    builder: &mut MtimeCacheBuilder,
) -> crate::Result<Arc<MacrosEnv>> {
    let database_url_env = crate::env_opt(config.common.database_url_var())?;
    let offline_dir_env = crate::env_opt("SQLX_OFFLINE_DIR")?.map(PathBuf::from);
    let offline_env = crate::env_opt("SQLX_OFFLINE")?.map(|val| is_truthy_bool(&val));

    // Don't load .env files if all environment variables are set: we may be in
    // a non-Cargo build system like buck2.
    let dotenv = if database_url_env.is_none() || offline_dir_env.is_none() || offline_env.is_none()
    {
        Some(load_from_dotenv(manifest_dir, config, builder)?)
    } else {
        None
    };

    Ok(Arc::new(MacrosEnv {
        // Make set variables take precedence
        database_url: database_url_env.or_else(|| dotenv.as_ref()?.database_url.clone()),
        offline_dir: offline_dir_env.or_else(|| dotenv.as_ref()?.offline_dir.clone()),
        offline: offline_env.or_else(|| dotenv.as_ref()?.offline),
    }))
}

/// Returns `true` if `val` is `"true"`,
fn is_truthy_bool(val: &str) -> bool {
    val.eq_ignore_ascii_case("true") || val == "1"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_from_dotenv_reads_env_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "DATABASE_URL=postgres://test\nSQLX_OFFLINE_DIR=/some/dir\nSQLX_OFFLINE=true\n",
        )
        .unwrap();

        let cache: MtimeCache<Arc<MacrosEnv>> = MtimeCache::new();
        let env = cache
            .get_or_try_init(|builder| load_from_dotenv(dir.path(), &Config::default(), builder))
            .unwrap();

        assert_eq!(env.database_url.as_deref(), Some("postgres://test"));
        assert_eq!(env.offline_dir, Some(PathBuf::from("/some/dir")));
        assert_eq!(env.offline, Some(true));
    }

    #[test]
    fn load_from_dotenv_empty_when_no_env_file() {
        // The ancestor walk finds nothing as long as no ancestor of the OS temp dir has a .env.
        let dir = tempfile::tempdir().unwrap();

        let cache: MtimeCache<Arc<MacrosEnv>> = MtimeCache::new();
        let env = cache
            .get_or_try_init(|builder| load_from_dotenv(dir.path(), &Config::default(), builder))
            .unwrap();

        assert!(env.database_url.is_none());
        assert!(env.offline_dir.is_none());
        assert!(env.offline.is_none());
    }

    #[test]
    fn resolve_workspace_root_prefers_override() {
        let dir = PathBuf::from("/fake/workspace");
        let result = resolve_workspace_root(Some(dir.clone()), || {
            panic!("cargo fallback must not be called when override is set")
        });
        assert_eq!(result, dir);
    }
}
