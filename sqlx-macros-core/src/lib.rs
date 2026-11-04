//! Support crate for SQLx's proc macros.
//!
//! ### Note: Semver Exempt API
//! The API of this crate is not meant for general use and does *not* follow Semantic Versioning.
//! The only crate that follows Semantic Versioning in the project is the `sqlx` crate itself.
//! If you are building a custom SQLx driver, you should pin an exact version of this and
//! `sqlx-core` to avoid breakages:
//!
//! ```toml
//! sqlx-core = "=0.6.2"
//! sqlx-macros-core = "=0.6.2"
//! ```
//!
//! And then make releases in lockstep with `sqlx-core`. We recommend all driver crates, in-tree
//! or otherwise, use the same version numbers as `sqlx-core` to avoid confusion.

#![cfg_attr(
    any(sqlx_macros_unstable, procmacro2_semver_exempt),
    feature(proc_macro_tracked_path, proc_macro_tracked_env)
)]

#[cfg(any(sqlx_macros_unstable, procmacro2_semver_exempt))]
extern crate proc_macro;

use cfg_if::cfg_if;
use std::path::PathBuf;

#[cfg(feature = "macros")]
use crate::query::QueryDriver;

pub type Error = Box<dyn std::error::Error>;

pub type Result<T, E = Error> = std::result::Result<T, E>;

mod common;
pub mod database;

#[cfg(feature = "derive")]
pub mod derives;
#[cfg(feature = "macros")]
pub mod query;

#[cfg(feature = "macros")]
// The compiler gives misleading help messages about `#[cfg(test)]` when this is just named `test`.
pub mod test_attr;

#[cfg(feature = "migrate")]
pub mod migrate;

#[cfg(feature = "macros")]
pub const FOSS_DRIVERS: &[QueryDriver] = &[
    #[cfg(feature = "mysql")]
    QueryDriver::new::<sqlx_mysql::MySql>(),
    #[cfg(feature = "postgres")]
    QueryDriver::new::<sqlx_postgres::Postgres>(),
    #[cfg(feature = "_sqlite")]
    QueryDriver::new::<sqlx_sqlite::Sqlite>(),
];

pub fn block_on<F>(f: F) -> F::Output
where
    F: std::future::Future,
{
    cfg_if! {
        if #[cfg(feature = "_rt-async-global-executor")] {
            sqlx_core::rt::test_block_on(f)
        } else if #[cfg(feature = "_rt-async-std")] {
            async_std::task::block_on(f)
        } else if #[cfg(feature = "_rt-smol")] {
            sqlx_core::rt::test_block_on(f)
        } else if #[cfg(feature = "_rt-tokio")] {
            use std::sync::LazyLock;

            use tokio::runtime::{self, Runtime};

            // We need a single, persistent Tokio runtime since we're caching connections,
            // otherwise we'll get "IO driver has terminated" errors.
            static TOKIO_RT: LazyLock<Runtime> = LazyLock::new(|| {
                runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to start Tokio runtime")
            });

            TOKIO_RT.block_on(f)
        } else {
            sqlx_core::rt::missing_rt(f)
        }
    }
}

pub fn env(var: &str) -> Result<String> {
    env_opt(var)?
        .ok_or_else(|| format!("env var {var:?} must be set to use the query macros").into())
}

#[allow(clippy::disallowed_methods)]
pub fn env_opt(var: &str) -> Result<Option<String>> {
    use std::env::VarError;

    #[cfg(any(sqlx_macros_unstable, procmacro2_semver_exempt))]
    let res: Result<String, VarError> = proc_macro::tracked::env_var(var);

    #[cfg(not(any(sqlx_macros_unstable, procmacro2_semver_exempt)))]
    let res: Result<String, VarError> = std::env::var(var);

    match res {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!("env var {var:?} is not valid UTF-8").into()),
    }
}

pub fn manifest_dir() -> Result<PathBuf> {
    Ok(env("CARGO_MANIFEST_DIR")?.into())
}
