use std::ffi::{c_int, CStr, CString};
use std::ptr::NonNull;
use std::{io, ptr};

use crate::error::Error;
use libsqlite3_sys::{
    sqlite3, sqlite3_close, sqlite3_exec, sqlite3_extended_result_codes, sqlite3_get_autocommit,
    sqlite3_last_insert_rowid, sqlite3_open_v2, SQLITE_OK,
};

use crate::SqliteError;

/// Managed SQLite3 database handle.
/// The database handle will be closed when this is dropped.
#[derive(Debug)]
pub(crate) struct ConnectionHandle(NonNull<sqlite3>);

// A SQLite3 handle is safe to send between threads, provided not more than
// one is accessing it at the same time. This is upheld as long as [SQLITE_CONFIG_MULTITHREAD] is
// enabled and [SQLITE_THREADSAFE] was enabled when sqlite was compiled. We refuse to work
// if these conditions are not upheld.
//
// <https://www.sqlite.org/c3ref/threadsafe.html>
// <https://www.sqlite.org/c3ref/c_config_covering_index_scan.html#sqliteconfigmultithread>

unsafe impl Send for ConnectionHandle {}

impl ConnectionHandle {
    pub(crate) fn open(filename: &CStr, flags: c_int) -> Result<Self, Error> {
        let mut handle = ptr::null_mut();

        // <https://www.sqlite.org/c3ref/open.html>
        let status = unsafe { sqlite3_open_v2(filename.as_ptr(), &mut handle, flags, ptr::null()) };

        // SAFETY: the database is still initialized as long as the pointer is not `NULL`.
        // We need to close it even if there's an error.
        let mut handle = Self(NonNull::new(handle).ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "SQLite is unable to allocate memory to hold the sqlite3 object",
            ))
        })?);

        if status != SQLITE_OK {
            return Err(Error::Database(Box::new(handle.expect_error())));
        }

        // Enable extended result codes
        // https://www.sqlite.org/c3ref/extended_result_codes.html
        unsafe {
            // This only returns a non-OK code if SQLite is built with `SQLITE_ENABLE_API_ARMOR`
            // and the database pointer is `NULL` or already closed.
            //
            // The invariants of this type guarantee that neither is true.
            sqlite3_extended_result_codes(handle.as_ptr(), 1);
        }

        Ok(handle)
    }

    #[inline]
    pub(crate) fn as_ptr(&self) -> *mut sqlite3 {
        self.0.as_ptr()
    }

    pub(crate) fn as_non_null_ptr(&self) -> NonNull<sqlite3> {
        self.0
    }

    pub(crate) fn call_with_result(
        &mut self,
        call: impl FnOnce(*mut sqlite3) -> c_int,
    ) -> Result<(), SqliteError> {
        let res = call(self.as_ptr());

        if res == SQLITE_OK {
            Ok(())
        } else {
            Err(self
                .last_error()
                .unwrap_or_else(|| SqliteError::from_code(res)))
        }
    }

    pub(crate) fn in_transaction(&mut self) -> bool {
        // SAFETY: we have exclusive access to the database handle
        let ret = unsafe { sqlite3_get_autocommit(self.as_ptr()) };
        ret == 0
    }

    pub(crate) fn last_insert_rowid(&mut self) -> i64 {
        // SAFETY: we have exclusive access to the database handle
        unsafe { sqlite3_last_insert_rowid(self.as_ptr()) }
    }

    pub(crate) fn last_error(&mut self) -> Option<SqliteError> {
        // SAFETY: we have exclusive access to the database handle
        unsafe { SqliteError::try_new(self.as_ptr()) }
    }

    #[track_caller]
    pub(crate) fn expect_error(&mut self) -> SqliteError {
        self.last_error()
            .expect("expected error code to be set in current context")
    }

    pub(crate) fn exec(&mut self, query: impl Into<String>) -> Result<(), Error> {
        let query = query.into();
        let query = CString::new(query).map_err(|_| err_protocol!("query contains nul bytes"))?;

        // SAFETY: we have exclusive access to the database handle
        unsafe {
            #[cfg_attr(not(feature = "unlock-notify"), expect(clippy::never_loop))]
            loop {
                let status = sqlite3_exec(
                    self.as_ptr(),
                    query.as_ptr(),
                    // callback if we wanted result rows
                    None,
                    // callback data
                    ptr::null_mut(),
                    // out-pointer for the error message, we just use `SqliteError::new()`
                    ptr::null_mut(),
                );

                match status {
                    SQLITE_OK => return Ok(()),
                    #[cfg(feature = "unlock-notify")]
                    libsqlite3_sys::SQLITE_LOCKED_SHAREDCACHE => {
                        crate::statement::unlock_notify::wait(self.as_ptr())?
                    }
                    _ => return Err(SqliteError::new(self.as_ptr()).into()),
                }
            }
        }
    }
}

impl Drop for ConnectionHandle {
    fn drop(&mut self) {
        unsafe {
            // https://sqlite.org/c3ref/close.html
            let status = sqlite3_close(self.0.as_ptr());
            if status != SQLITE_OK {
                // this should *only* happen due to an internal bug in SQLite where we left
                // SQLite handles open
                panic!("{}", SqliteError::new(self.0.as_ptr()));
            }
        }
    }
}
