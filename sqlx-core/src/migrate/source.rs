use crate::error::BoxDynError;
use crate::migrate::{migration, Migration, MigrationType};
use crate::sql_str::{AssertSqlSafe, SqlSafeStr};
use futures_core::future::BoxFuture;

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// In the default implementation, a MigrationSource is a directory which
/// contains the migration SQL scripts. All these scripts must be stored in
/// files with names using the format `<VERSION>_<DESCRIPTION>.sql`, where
/// `<VERSION>` is a string that can be parsed into `i64` and its value is
/// greater than zero, and `<DESCRIPTION>` is a string.
///
/// Files that don't end in `.sql` are silently ignored. A `.sql` file that doesn't match this
/// format is an error, since it is almost certainly a migration that was named incorrectly.
///
/// You can create a new empty migration script using sqlx-cli:
/// `sqlx migrate add <DESCRIPTION>`.
///
/// Note that migrations for each database are tracked using the
/// `_sqlx_migrations` table (stored in the database). If a migration's hash
/// changes and it has already been run, this will cause an error.
pub trait MigrationSource<'s>: Debug {
    fn resolve(self) -> BoxFuture<'s, Result<Vec<Migration>, BoxDynError>>;
}

impl<'s> MigrationSource<'s> for &'s Path {
    fn resolve(self) -> BoxFuture<'s, Result<Vec<Migration>, BoxDynError>> {
        // Behavior changed from previous because `canonicalize()` is potentially blocking
        // since it might require going to disk to fetch filesystem data.
        self.to_owned().resolve()
    }
}

impl MigrationSource<'static> for PathBuf {
    fn resolve(self) -> BoxFuture<'static, Result<Vec<Migration>, BoxDynError>> {
        // Technically this could just be `Box::pin(spawn_blocking(...))`
        // but that would actually be a breaking behavior change because it would call
        // `spawn_blocking()` on the current thread
        Box::pin(async move {
            crate::rt::spawn_blocking(move || {
                let migrations_with_paths = resolve_blocking(&self)?;

                Ok(migrations_with_paths.into_iter().map(|(m, _p)| m).collect())
            })
            .await
        })
    }
}

/// A [`MigrationSource`] implementation with configurable resolution.
///
/// `S` may be `PathBuf`, `&Path` or any type that implements `Into<PathBuf>`.
///
/// See [`ResolveConfig`] for details.
#[derive(Debug)]
pub struct ResolveWith<S>(pub S, pub ResolveConfig);

impl<'s, S: Debug + Into<PathBuf> + Send + 's> MigrationSource<'s> for ResolveWith<S> {
    fn resolve(self) -> BoxFuture<'s, Result<Vec<Migration>, BoxDynError>> {
        Box::pin(async move {
            let path = self.0.into();
            let config = self.1;

            let migrations_with_paths =
                crate::rt::spawn_blocking(move || resolve_blocking_with_config(&path, &config))
                    .await?;

            Ok(migrations_with_paths.into_iter().map(|(m, _p)| m).collect())
        })
    }
}

#[derive(thiserror::Error, Debug)]
#[error("{message}")]
pub struct ResolveError {
    message: String,
    #[source]
    source: Option<io::Error>,
}

/// Configuration for migration resolution using [`ResolveWith`].
#[derive(Debug, Default)]
pub struct ResolveConfig {
    ignored_chars: BTreeSet<char>,
}

impl ResolveConfig {
    /// Return a default, empty configuration.
    pub fn new() -> Self {
        ResolveConfig {
            ignored_chars: BTreeSet::new(),
        }
    }

    /// Ignore a character when hashing migrations.
    ///
    /// The migration SQL string itself will still contain the character,
    /// but it will not be included when calculating the checksum.
    ///
    /// This can be used to ignore whitespace characters so changing formatting
    /// does not change the checksum.
    ///
    /// Adding the same `char` more than once is a no-op.
    ///
    /// ### Note: Changes Migration Checksum
    /// This will change the checksum of resolved migrations,
    /// which may cause problems with existing deployments.
    ///
    /// **Use at your own risk.**
    pub fn ignore_char(&mut self, c: char) -> &mut Self {
        self.ignored_chars.insert(c);
        self
    }

    /// Ignore one or more characters when hashing migrations.
    ///
    /// The migration SQL string itself will still contain these characters,
    /// but they will not be included when calculating the checksum.
    ///
    /// This can be used to ignore whitespace characters so changing formatting
    /// does not change the checksum.
    ///
    /// Adding the same `char` more than once is a no-op.
    ///
    /// ### Note: Changes Migration Checksum
    /// This will change the checksum of resolved migrations,
    /// which may cause problems with existing deployments.
    ///
    /// **Use at your own risk.**
    pub fn ignore_chars(&mut self, chars: impl IntoIterator<Item = char>) -> &mut Self {
        self.ignored_chars.extend(chars);
        self
    }

    /// Iterate over the set of ignored characters.
    ///
    /// Duplicate `char`s are not included.
    pub fn ignored_chars(&self) -> impl Iterator<Item = char> + '_ {
        self.ignored_chars.iter().copied()
    }
}

// FIXME: paths should just be part of `Migration` but we can't add a field backwards compatibly
// since it's `#[non_exhaustive]`.
#[doc(hidden)]
pub fn resolve_blocking(path: &Path) -> Result<Vec<(Migration, PathBuf)>, ResolveError> {
    resolve_blocking_with_config(path, &ResolveConfig::new())
}

#[doc(hidden)]
pub fn resolve_blocking_with_config(
    path: &Path,
    config: &ResolveConfig,
) -> Result<Vec<(Migration, PathBuf)>, ResolveError> {
    let path = path.canonicalize().map_err(|e| ResolveError {
        message: format!("error canonicalizing path {}", path.display()),
        source: Some(e),
    })?;

    let s = fs::read_dir(&path).map_err(|e| ResolveError {
        message: format!("error reading migration directory {}", path.display()),
        source: Some(e),
    })?;

    let mut migrations = Vec::new();

    for res in s {
        let entry = res.map_err(|e| ResolveError {
            message: format!(
                "error reading contents of migration directory {}",
                path.display()
            ),
            source: Some(e),
        })?;

        let entry_path = entry.path();

        let metadata = fs::metadata(&entry_path).map_err(|e| ResolveError {
            message: format!(
                "error getting metadata of migration path {}",
                entry_path.display()
            ),
            source: Some(e),
        })?;

        if !metadata.is_file() {
            // not a file; ignore
            continue;
        }

        let file_name = entry.file_name();
        // This is arguably the wrong choice,
        // but it really only matters for parsing the version and description.
        //
        // Using `.to_str()` and returning an error if the filename is not UTF-8
        // would be a breaking change.
        let file_name = file_name.to_string_lossy();

        let parts = file_name.splitn(2, '_').collect::<Vec<_>>();

        if parts.len() != 2 || !parts[1].ends_with(".sql") {
            if file_name.ends_with(".sql") {
                // A `.sql` file that doesn't parse is almost certainly a migration that was
                // named incorrectly, so erroring is more useful than silently skipping it.
                return Err(ResolveError {
                    message: format!(
                        "error parsing migration filename {file_name:?}; \
                         expected the format `<VERSION>_<DESCRIPTION>.sql` (e.g. `01_foo.sql`)"
                    ),
                    source: None,
                });
            }

            // not of the format: <VERSION>_<DESCRIPTION>.<REVERSIBLE_DIRECTION>.sql; ignore
            continue;
        }

        let version: i64 = parts[0].parse()
            .map_err(|_e| ResolveError {
                message: format!("error parsing migration filename {file_name:?}; expected integer version prefix (e.g. `01_foo.sql`)"),
                source: None,
            })?;

        let migration_type = MigrationType::from_filename(parts[1]);

        // remove the `.sql` and replace `_` with ` `
        let description = parts[1]
            .trim_end_matches(migration_type.suffix())
            .replace('_', " ")
            .to_owned();

        let sql = fs::read_to_string(&entry_path).map_err(|e| ResolveError {
            message: format!(
                "error reading contents of migration {}: {e}",
                entry_path.display()
            ),
            source: Some(e),
        })?;

        // opt-out of migration transaction
        let no_tx = sql.starts_with("-- no-transaction");

        let checksum = checksum_with(&sql, &config.ignored_chars);

        migrations.push((
            Migration::with_checksum(
                version,
                Cow::Owned(description),
                migration_type,
                AssertSqlSafe(sql).into_sql_str(),
                checksum.into(),
                no_tx,
            ),
            entry_path,
        ));
    }

    migrations.sort();

    Ok(migrations)
}

fn checksum_with(sql: &str, ignored_chars: &BTreeSet<char>) -> Vec<u8> {
    if ignored_chars.is_empty() {
        // This is going to be much faster because it doesn't have to UTF-8 decode `sql`.
        return migration::checksum(sql);
    }

    migration::checksum_fragments(sql.split(|c| ignored_chars.contains(&c)))
}

#[test]
fn checksum_with_ignored_chars() {
    // Ensure that `checksum_with` returns the same digest for a given set of ignored chars
    // as the equivalent string with the characters removed.
    let ignored_chars = [
        ' ', '\t', '\r', '\n',
        // Zero-width non-breaking space (ZWNBSP), often added as a magic-number at the beginning
        // of UTF-8 encoded files as a byte-order mark (BOM):
        // https://en.wikipedia.org/wiki/Byte_order_mark
        '\u{FEFF}',
    ];

    // Copied from `examples/postgres/axum-social-with-tests/migrations/3_comment.sql`
    let sql = "\
        \u{FEFF}create table comment (\r\n\
            \tcomment_id uuid primary key default gen_random_uuid(),\r\n\
            \tpost_id uuid not null references post(post_id),\r\n\
            \tuser_id uuid not null references \"user\"(user_id),\r\n\
            \tcontent text not null,\r\n\
            \tcreated_at timestamptz not null default now()\r\n\
        );\r\n\
        \r\n\
        create index on comment(post_id, created_at);\r\n\
    ";

    let stripped_sql = sql.replace(&ignored_chars[..], "");

    let ignored_chars = BTreeSet::from(ignored_chars);

    let digest_ignored = checksum_with(sql, &ignored_chars);
    let digest_stripped = migration::checksum(&stripped_sql);

    assert_eq!(digest_ignored, digest_stripped);
}

#[test]
fn resolve_errors_on_sql_file_without_version_prefix() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("schema.sql"), "create table foo();").unwrap();

    let error = resolve_blocking(dir.path())
        .expect_err("expected an error for a `.sql` file with no version prefix");

    assert!(
        error.to_string().contains("schema.sql"),
        "error should name the offending file, got: {error}"
    );
}

#[test]
fn resolve_ignores_files_not_ending_in_sql() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("README.md"), "these are the migrations").unwrap();
    fs::write(dir.path().join(".gitkeep"), "").unwrap();
    fs::write(dir.path().join("1_foo.sql"), "create table foo();").unwrap();

    let migrations = resolve_blocking(dir.path()).unwrap();

    assert_eq!(migrations.len(), 1);
    assert_eq!(migrations[0].0.version, 1);
    assert_eq!(migrations[0].0.description, "foo");
}
