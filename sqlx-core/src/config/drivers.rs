use std::error::Error;

/// Configuration for specific database drivers (**applies to macros and `sqlx-cli` only**).
///
/// # Note: Does Not Apply at Application Run-Time
/// As of writing, these configuration parameters do *not* have any bearing on
/// the runtime configuration of SQLx database drivers.
///
/// Any parameters which overlap with runtime configuration
/// (e.g. [`drivers.sqlite.unsafe-load-extensions`][SqliteConfig::unsafe_load_extensions])
/// _must_ be configured their normal ways at runtime (e.g. `SqliteConnectOptions::extension()`).
///
/// See the documentation of individual fields for details.
#[derive(Debug, Default)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(default, rename_all = "kebab-case", deny_unknown_fields)
)]
pub struct Config {
    /// Configuration for the MySQL database driver.
    ///
    /// See [`MySqlConfig`] for details.
    pub mysql: MySqlConfig,

    /// Configuration for the Postgres database driver.
    ///
    /// See [`PgConfig`] for details.
    pub postgres: PgConfig,

    /// Configuration for the SQLite database driver.
    ///
    /// See [`SqliteConfig`] for details.
    pub sqlite: SqliteConfig,

    /// Configuration for external database drivers.
    ///
    /// See [`ExternalDriverConfig`] for details.
    pub external: ExternalDriverConfig,
}

/// Configuration for the MySQL database driver.
#[derive(Debug, Default)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(default, rename_all = "kebab-case", deny_unknown_fields)
)]
pub struct MySqlConfig {
    // No fields implemented yet. This key is only used to validate parsing.
}

/// Configuration for the Postgres database driver.
#[derive(Debug, Default)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(default, rename_all = "kebab-case", deny_unknown_fields)
)]
pub struct PgConfig {
    // No fields implemented yet. This key is only used to validate parsing.
}

/// Configuration for the SQLite database driver.
#[derive(Debug, Default)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(default, rename_all = "kebab-case", deny_unknown_fields)
)]
pub struct SqliteConfig {
    /// Specify extensions to load, either by name or by path.
    ///
    /// Paths should be relative to the workspace root.
    ///
    /// See [Loading an Extension](https://www.sqlite.org/loadext.html#loading_an_extension)
    /// in the SQLite manual for details.
    ///
    /// The `sqlite-load-extension` feature must be enabled and SQLite must be built
    /// _without_ [`SQLITE_OMIT_LOAD_EXTENSION`] enabled.
    ///
    /// [`SQLITE_OMIT_LOAD_EXTENSION`]: https://www.sqlite.org/compile.html#omit_load_extension
    ///
    /// # Note: Does Not Configure Runtime Extension Loading
    /// Extensions to be loaded at runtime *must* be separately configured with
    /// `SqliteConnectOptions::extension()` or `SqliteConnectOptions::extension_with_entrypoint()`.
    ///
    /// # Safety
    /// This causes arbitrary DLLs on the filesystem to be loaded at execution time,
    /// which can easily result in undefined behavior, memory corruption,
    /// or exploitable vulnerabilities if misused.
    ///
    /// It is not possible to provide a truly safe version of this API.
    ///
    /// Use this field with care, and only load extensions that you trust.
    ///
    /// # Example
    /// Load the `uuid` and `vsv` extensions from [`sqlean`](https://github.com/nalgeon/sqlean).
    ///
    /// `sqlx.toml`:
    /// ```toml
    /// [common.drivers.sqlite]
    /// unsafe-load-extensions = ["uuid", "vsv"]
    /// ```
    pub unsafe_load_extensions: Vec<SqliteExtension>,
}

/// Extension for the SQLite database driver.
#[derive(Debug, PartialEq)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(untagged, deny_unknown_fields)
)]
pub enum SqliteExtension {
    Path(String),
    PathWithEntrypoint { path: String, entrypoint: String },
}

/// Configuration for external database drivers.
#[derive(Debug, Default)]
#[cfg_attr(feature = "sqlx-toml", derive(serde::Deserialize), serde(transparent))]
pub struct ExternalDriverConfig {
    #[cfg(feature = "sqlx-toml")]
    by_name: std::collections::BTreeMap<String, toml::Table>,
}

/// Type-erased [`toml::de::Error`].
pub type TryParseError = Box<dyn Error + Send + Sync + 'static>;

impl ExternalDriverConfig {
    /// Try to parse the config for a given driver name, returning `Ok(None)` if it does not exist.
    #[cfg(feature = "sqlx-toml")]
    pub fn try_parse<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
    ) -> Result<Option<T>, TryParseError> {
        let Some(config) = self.by_name.get(name) else {
            return Ok(None);
        };

        // What's really baffling is that `toml` doesn't provide any way to deserialize
        // from a `&Table` or `&Value`, only owned variants, so cloning is unavoidable here.
        Ok(Some(config.clone().try_into()?))
    }

    /// Try to parse the config for a given driver name, returning `Ok(None)` if it does not exist.
    #[cfg(not(feature = "sqlx-toml"))]
    pub fn try_parse<T>(&self, _name: &str) -> Result<Option<T>, TryParseError> {
        Ok(None)
    }
}
