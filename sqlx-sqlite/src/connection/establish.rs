use crate::connection::handle::ConnectionHandle;
use crate::connection::LogSettings;
use crate::connection::{ConnectionState, Statements};
use crate::error::Error;
use crate::SqliteConnectOptions;
use libsqlite3_sys::{
    sqlite3_busy_timeout, SQLITE_OPEN_CREATE, SQLITE_OPEN_FULLMUTEX, SQLITE_OPEN_MEMORY,
    SQLITE_OPEN_NOMUTEX, SQLITE_OPEN_PRIVATECACHE, SQLITE_OPEN_READONLY, SQLITE_OPEN_READWRITE,
    SQLITE_OPEN_SHAREDCACHE, SQLITE_OPEN_URI,
};
use percent_encoding::NON_ALPHANUMERIC;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[cfg(feature = "load-extension")]
use sqlx_core::IndexMap;

// This was originally `AtomicU64` but that's not supported on MIPS (or PowerPC):
// https://github.com/launchbadge/sqlx/issues/2859
// https://doc.rust-lang.org/stable/std/sync/atomic/index.html#portability
static THREAD_ID: AtomicUsize = AtomicUsize::new(0);

pub struct EstablishParams {
    filename: CString,
    open_flags: i32,
    busy_timeout: Duration,
    statement_cache_capacity: usize,
    log_settings: LogSettings,
    #[cfg(feature = "load-extension")]
    extensions: IndexMap<CString, Option<CString>>,
    pub(crate) thread_name: String,
    pub(crate) command_channel_size: usize,
    #[cfg(feature = "regexp")]
    register_regexp_function: bool,
}

impl EstablishParams {
    pub fn from_options(options: &SqliteConnectOptions) -> Result<Self, Error> {
        let mut filename = options
            .filename
            .to_str()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filename passed to SQLite must be valid UTF-8",
                )
            })?
            .to_owned();

        // Set common flags we expect to have in sqlite
        let mut flags = SQLITE_OPEN_URI;

        // By default, we connect to an in-memory database.
        // [SQLITE_OPEN_NOMUTEX] will instruct [sqlite3_open_v2] to return an error if it
        // cannot satisfy our wish for a thread-safe, lock-free connection object

        flags |= if options.serialized {
            SQLITE_OPEN_FULLMUTEX
        } else {
            SQLITE_OPEN_NOMUTEX
        };

        flags |= if options.read_only {
            SQLITE_OPEN_READONLY
        } else if options.create_if_missing {
            SQLITE_OPEN_CREATE | SQLITE_OPEN_READWRITE
        } else {
            SQLITE_OPEN_READWRITE
        };

        if options.in_memory {
            flags |= SQLITE_OPEN_MEMORY;
        }

        flags |= if options.shared_cache {
            SQLITE_OPEN_SHAREDCACHE
        } else {
            SQLITE_OPEN_PRIVATECACHE
        };

        let mut query_params = BTreeMap::new();

        if options.immutable {
            query_params.insert("immutable", "true");
        }

        if let Some(vfs) = options.vfs.as_deref() {
            query_params.insert("vfs", vfs);
        }

        if !query_params.is_empty() {
            filename = format!(
                "file:{}?",
                percent_encoding::percent_encode(filename.as_bytes(), NON_ALPHANUMERIC),
            );

            // Suffix serializer automatically handles `&` separators for us.
            let filename_len = filename.len();
            filename = form_urlencoded::Serializer::for_suffix(filename, filename_len)
                .extend_pairs(query_params)
                .finish();
        }

        let filename = CString::new(filename).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "filename passed to SQLite must not contain nul bytes",
            )
        })?;

        #[cfg(feature = "load-extension")]
        let extensions = options
            .extensions
            .iter()
            .map(|(name, entry)| {
                let entry = entry
                    .as_ref()
                    .map(|e| {
                        CString::new(e.as_bytes()).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "extension entrypoint names passed to SQLite must not contain nul bytes"
                            )
                        })
                    })
                    .transpose()?;
                Ok((
                    CString::new(name.as_bytes()).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "extension names passed to SQLite must not contain nul bytes",
                        )
                    })?,
                    entry,
                ))
            })
            .collect::<Result<IndexMap<CString, Option<CString>>, io::Error>>()?;

        let thread_id = THREAD_ID.fetch_add(1, Ordering::AcqRel);

        Ok(Self {
            filename,
            open_flags: flags,
            busy_timeout: options.busy_timeout,
            statement_cache_capacity: options.statement_cache_capacity,
            log_settings: options.log_settings.clone(),
            #[cfg(feature = "load-extension")]
            extensions,
            thread_name: (options.thread_name)(thread_id as u64),
            command_channel_size: options.command_channel_size,
            #[cfg(feature = "regexp")]
            register_regexp_function: options.register_regexp_function,
        })
    }

    pub(crate) fn establish(&self) -> Result<ConnectionState, Error> {
        let mut handle = ConnectionHandle::open(&self.filename, self.open_flags)?;

        #[cfg(feature = "load-extension")]
        unsafe {
            self.apply_extensions(&mut handle)?;
        }

        #[cfg(feature = "regexp")]
        if self.register_regexp_function {
            // configure a `regexp` function for sqlite, it does not come with one by default
            let status = crate::regexp::register(handle.as_ptr());
            if status != libsqlite3_sys::SQLITE_OK {
                return Err(Error::Database(Box::new(handle.expect_error())));
            }
        }

        // Configure a busy timeout
        // This causes SQLite to automatically sleep in increasing intervals until the time
        // when there is something locked during [sqlite3_step].
        //
        // We also need to convert the u128 value to i32, checking we're not overflowing.
        let ms = i32::try_from(self.busy_timeout.as_millis())
            .expect("Given busy timeout value is too big.");

        handle.call_with_result(|db| unsafe { sqlite3_busy_timeout(db, ms) })?;

        Ok(ConnectionState {
            handle,
            statements: Statements::new(self.statement_cache_capacity),
            log_settings: self.log_settings.clone(),
            progress_handler_callback: None,
            update_hook_callback: None,
            #[cfg(feature = "preupdate-hook")]
            preupdate_hook_callback: None,
            commit_hook_callback: None,
            rollback_hook_callback: None,
        })
    }

    #[cfg(feature = "load-extension")]
    unsafe fn apply_extensions(&self, handle: &mut ConnectionHandle) -> Result<(), Error> {
        use libsqlite3_sys::{sqlite3_free, sqlite3_load_extension};
        use std::ffi::{c_int, CStr};
        use std::ptr;

        /// `true` enables *just* `sqlite3_load_extension`, false disables *all* extension loading.
        fn enable_load_extension(
            handle: &mut ConnectionHandle,
            enabled: bool,
        ) -> Result<(), Error> {
            use libsqlite3_sys::{sqlite3_db_config, SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION};

            // SAFETY: we have exclusive access and this matches the expected signature
            // <https://www.sqlite.org/c3ref/c_dbconfig_defensive.html#sqlitedbconfigenableloadextension>
            handle.call_with_result(|db| unsafe {
                // https://doc.rust-lang.org/reference/expressions/operator-expr.html#r-expr.as.bool-char-as-int
                sqlite3_db_config(
                    db,
                    SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION,
                    enabled as c_int,
                    ptr::null_mut::<c_int>(),
                )
            })?;

            Ok(())
        }

        if self.extensions.is_empty() {
            return Ok(());
        }

        // We enable extension loading only so long as *we're* doing it.
        enable_load_extension(handle, true)?;

        for (name, entrypoint) in &self.extensions {
            let name_ptr = name.as_ptr();
            let entrypoint_ptr = entrypoint.as_ref().map_or_else(ptr::null, |s| s.as_ptr());
            let mut err_msg_ptr = ptr::null_mut();

            // SAFETY:
            // * we have exclusive access
            // * all pointers are initialized
            // * we warn the user about loading extensions in documentation
            handle
                .call_with_result(|db| unsafe {
                    sqlite3_load_extension(db, name_ptr, entrypoint_ptr, &mut err_msg_ptr)
                })
                .map_err(|e| {
                    if !err_msg_ptr.is_null() {
                        // SAFETY: pointer is not-null,
                        // and we copy the error message to an allocation we own.
                        let err_msg = unsafe { CStr::from_ptr(err_msg_ptr) }
                            // In practice, the string *should* be UTF-8.
                            .to_string_lossy()
                            .into_owned();

                        // SAFETY: we're expected to free the error message afterward.
                        unsafe {
                            sqlite3_free(err_msg_ptr.cast());
                        }

                        e.with_message(err_msg)
                    } else {
                        e
                    }
                })?;
        }

        // We then disable extension loading immediately afterward.
        enable_load_extension(handle, false)
    }
}
