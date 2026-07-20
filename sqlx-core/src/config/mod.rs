//! (Exported for documentation only) Guide and reference for `sqlx.toml` files.
//!
//! To use, create a `sqlx.toml` file in your crate root (the same directory as your `Cargo.toml`).
//! The configuration in a `sqlx.toml` configures SQLx *only* for the current crate.
//!
//! Requires the `sqlx-toml` feature (not enabled by default).
//!
//! `sqlx-cli` will also read `sqlx.toml` when running migrations.
//!
//! See the [`Config`] type and its fields for individual configuration options.
//!
//! See the [reference][`_reference`] for the full `sqlx.toml` file.

use std::error::Error;
use std::fmt::Debug;
use std::io;
use std::path::{Path, PathBuf};

/// Configuration shared by multiple components.
///
/// See [`common::Config`] for details.
pub mod common;

pub mod drivers;

/// Configuration for the `query!()` family of macros.
///
/// See [`macros::Config`] for details.
pub mod macros;

/// Configuration for migrations when executed using `sqlx::migrate!()` or through `sqlx-cli`.
///
/// See [`migrate::Config`] for details.
pub mod migrate;

/// Reference for `sqlx.toml` files
///
/// Source: `sqlx-core/src/config/reference.toml`
///
/// ```toml
#[doc = include_str!("reference.toml")]
/// ```
pub mod _reference {}

#[cfg(all(test, feature = "sqlx-toml"))]
mod tests;

/// The parsed structure of a `sqlx.toml` file.
#[derive(Debug, Default)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(default, rename_all = "kebab-case", deny_unknown_fields)
)]
pub struct Config {
    /// Configuration shared by multiple components.
    ///
    /// See [`common::Config`] for details.
    pub common: common::Config,

    /// Configuration for database drivers.
    ///
    /// See [`drivers::Config`] for details.
    pub drivers: drivers::Config,

    /// Configuration for the `query!()` family of macros.
    ///
    /// See [`macros::Config`] for details.
    pub macros: macros::Config,

    /// Configuration for migrations when executed using `sqlx::migrate!()` or through `sqlx-cli`.
    ///
    /// See [`migrate::Config`] for details.
    pub migrate: migrate::Config,
}

/// Error returned from various methods of [`Config`].
#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    /// The loading method expected `CARGO_MANIFEST_DIR` to be set and it wasn't.
    ///
    /// This is necessary to locate the root of the crate currently being compiled.
    ///
    /// See [the "Environment Variables" page of the Cargo Book][cargo-env] for details.
    ///
    /// [cargo-env]: https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates
    #[error("environment variable `CARGO_MANIFEST_DIR` must be set and valid")]
    Env(
        #[from]
        #[source]
        std::env::VarError,
    ),

    /// No configuration file was found. Not necessarily fatal.
    #[error("config file {path:?} not found")]
    NotFound { path: PathBuf },

    /// An I/O error occurred while attempting to read the config file at `path`.
    ///
    /// If the error is [`io::ErrorKind::NotFound`], [`Self::NotFound`] is returned instead.
    #[error("error reading config file {path:?}")]
    Io {
        path: PathBuf,
        #[source]
        error: io::Error,
    },

    /// An error in the TOML was encountered while parsing the config file at `path`.
    ///
    /// The error gives line numbers and context when printed with `Display`/`ToString`.
    ///
    /// Only returned if the `sqlx-toml` feature is enabled.
    #[error("error parsing config file {path:?}")]
    Parse {
        path: PathBuf,
        /// Type-erased [`toml::de::Error`].
        #[source]
        error: Box<dyn Error + Send + Sync + 'static>,
    },

    /// A `sqlx.toml` file was found or specified, but the `sqlx-toml` feature is not enabled.
    #[error("SQLx found config file at {path:?} but the `sqlx-toml` feature was not enabled")]
    ParseDisabled { path: PathBuf },
}

impl ConfigError {
    /// Create a [`ConfigError`] from a [`std::io::Error`].
    ///
    /// Maps to either `NotFound` or `Io`.
    pub fn from_io(path: impl Into<PathBuf>, error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::NotFound { path: path.into() }
        } else {
            Self::Io {
                path: path.into(),
                error,
            }
        }
    }

    /// If this error means the file was not found, return the path that was attempted.
    pub fn not_found_path(&self) -> Option<&Path> {
        if let Self::NotFound { path } = self {
            Some(path)
        } else {
            None
        }
    }
}

/// Internal methods for loading a `Config`.
#[allow(clippy::result_large_err)]
impl Config {
    /// Read `$CARGO_MANIFEST_DIR/sqlx.toml` or return `Config::default()` if it does not exist.
    ///
    /// # Errors
    /// * If `CARGO_MANIFEST_DIR` is not set.
    /// * If the file exists but could not be read or parsed.
    /// * If the file exists but the `sqlx-toml` feature is disabled.
    pub fn try_from_crate_or_default() -> Result<Self, ConfigError> {
        Self::try_from_path_or_default(get_crate_path()?)
    }

    /// Attempt to read `Config` from the path given, or return `Config::default()` if it does not exist.
    ///
    /// # Errors
    /// * If the file exists but could not be read or parsed.
    /// * If the file exists but the `sqlx-toml` feature is disabled.
    pub fn try_from_path_or_default(path: PathBuf) -> Result<Self, ConfigError> {
        Self::read_from(path).or_else(|e| {
            if let ConfigError::NotFound { .. } = e {
                Ok(Config::default())
            } else {
                Err(e)
            }
        })
    }

    /// Attempt to read `Config` from the path given.
    ///
    /// # Errors
    /// * If the file does not exist.
    /// * If the file exists but could not be read or parsed.
    /// * If the file exists but the `sqlx-toml` feature is disabled.
    pub fn try_from_path(path: PathBuf) -> Result<Self, ConfigError> {
        Self::read_from(path)
    }

    #[cfg(feature = "sqlx-toml")]
    fn read_from(path: PathBuf) -> Result<Self, ConfigError> {
        // The `toml` crate doesn't provide an incremental reader.
        let toml_s = match std::fs::read_to_string(&path) {
            Ok(toml) => toml,
            Err(error) => {
                return Err(ConfigError::from_io(path, error));
            }
        };

        // TODO: parse and lint TOML structure before deserializing
        // Motivation: https://github.com/toml-rs/toml/issues/761
        tracing::debug!("read config TOML from {path:?}:\n{toml_s}");

        toml::from_str(&toml_s).map_err(|error| ConfigError::Parse {
            path,
            error: Box::new(error),
        })
    }

    #[cfg(not(feature = "sqlx-toml"))]
    fn read_from(path: PathBuf) -> Result<Self, ConfigError> {
        match path.try_exists() {
            Ok(true) => Err(ConfigError::ParseDisabled { path }),
            Ok(false) => Err(ConfigError::NotFound { path }),
            Err(e) => Err(ConfigError::from_io(path, e)),
        }
    }
}

fn get_crate_path() -> Result<PathBuf, ConfigError> {
    let mut path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    path.push("sqlx.toml");
    Ok(path)
}
