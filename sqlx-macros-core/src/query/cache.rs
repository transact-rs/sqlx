use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// A cached value derived from one or more files, which is automatically invalidated
/// if the modified-time of any watched file changes.
pub struct MtimeCache<T> {
    inner: Mutex<Option<MtimeCacheInner<T>>>,
}

pub struct MtimeCacheBuilder {
    file_mtimes: Vec<(PathBuf, Option<SystemTime>)>,
}

struct MtimeCacheInner<T> {
    builder: MtimeCacheBuilder,
    cached: T,
}

impl<T: Clone> MtimeCache<T> {
    pub fn new() -> Self {
        MtimeCache {
            inner: Mutex::new(None),
        }
    }

    /// Get the cached value, or (re)initialize it if it does not exist or a file's mtime has changed.
    pub fn get_or_try_init<E>(
        &self,
        init: impl FnOnce(&mut MtimeCacheBuilder) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| {
            // Reset the cache on-panic.
            let mut locked = e.into_inner();
            *locked = None;
            locked
        });

        if let Some(inner) = &*inner {
            if !inner.builder.any_modified() {
                return Ok(inner.cached.clone());
            }
        }

        let mut builder = MtimeCacheBuilder::new();

        let value = init(&mut builder)?;

        *inner = Some(MtimeCacheInner {
            builder,
            cached: value.clone(),
        });

        Ok(value)
    }
}

impl MtimeCacheBuilder {
    fn new() -> Self {
        MtimeCacheBuilder {
            file_mtimes: Vec::new(),
        }
    }

    /// Add a file path to watch.
    ///
    /// The cached value will be automatically invalidated if the modified-time of the file changes,
    /// or if the file does not exist but is created sometime after this call.
    pub fn add_path(&mut self, path: PathBuf) {
        let mtime = get_mtime(&path);

        #[cfg(any(sqlx_macros_unstable, procmacro2_semver_exempt))]
        {
            proc_macro::tracked::path(&path);
        }

        self.file_mtimes.push((path, mtime));
    }

    fn any_modified(&self) -> bool {
        for (path, expected_mtime) in &self.file_mtimes {
            let actual_mtime = get_mtime(path);

            if expected_mtime != &actual_mtime {
                return true;
            }
        }

        false
    }
}

fn get_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}
