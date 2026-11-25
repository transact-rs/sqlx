use std::ffi::{c_int, c_void, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::{slice, sync::Arc};

use libsqlite3_sys as ffi;
use sqlx_core::common::DebugFn;
use sqlx_core::decode::Decode;
use sqlx_core::encode::Encode;
use sqlx_core::error::{BoxDynError, Error};

use super::handle::ConnectionHandle;
use crate::{Sqlite, SqliteArgumentValue, SqliteError, SqliteValueRef};

type Callback = Arc<
    DebugFn<
        dyn Fn(&mut SqliteFunctionArgs<'_>) -> Result<SqliteArgumentValue, BoxDynError>
            + Send
            + Sync,
    >,
>;

/// An application-defined SQLite scalar function registered on each connection.
#[derive(Clone, Debug)]
pub struct SqliteFunction {
    name: Arc<str>,
    num_args: i32,
    deterministic: bool,
    callback: Callback,
}

impl SqliteFunction {
    /// Create a function taking `num_args` arguments, or `-1` for a variable count.
    /// Fixed argument counts range from 0 to 127. SQLite function names are at most
    /// 255 UTF-8 bytes and cannot contain a NUL character.
    ///
    /// A callback error becomes an SQL error. Database schema objects such as views
    /// and triggers can invoke these functions, so take care when exposing callbacks
    /// with side effects to untrusted database files.
    pub fn new<F, T>(name: impl Into<Arc<str>>, num_args: i32, callback: F) -> Self
    where
        F: Fn(&mut SqliteFunctionArgs<'_>) -> Result<T, BoxDynError> + Send + Sync + 'static,
        T: for<'q> Encode<'q, Sqlite>,
    {
        Self {
            name: name.into(),
            num_args,
            deterministic: false,
            callback: Arc::new(DebugFn(move |args: &mut SqliteFunctionArgs<'_>| {
                SqliteArgumentValue::encode_one(callback(args)?)
            })),
        }
    }

    /// Tell SQLite the function always returns the same result for the same inputs.
    /// This allows its use in expression indexes and generated columns.
    #[must_use]
    pub fn deterministic(mut self) -> Self {
        self.deterministic = true;
        self
    }

    pub(crate) fn create(&self, handle: &mut ConnectionHandle) -> Result<(), Error> {
        if !(-1..=127).contains(&self.num_args) {
            return Err(err_protocol!(
                "invalid SQLite function arity: {}",
                self.num_args
            ));
        }
        let name = CString::new(&*self.name)
            .map_err(|_| err_protocol!("SQLite function name contains a NUL byte"))?;
        let mut flags = ffi::SQLITE_UTF8;
        if self.deterministic {
            flags |= ffi::SQLITE_DETERMINISTIC;
        }

        // Each connection owns one callback reference. SQLite calls destroy on
        // replacement, close, and registration failure.
        let data = Box::into_raw(Box::new(Arc::clone(&self.callback)));
        let status = unsafe {
            ffi::sqlite3_create_function_v2(
                handle.as_ptr(),
                name.as_ptr(),
                self.num_args,
                flags,
                data.cast(),
                Some(call),
                None,
                None,
                Some(destroy),
            )
        };
        if status != ffi::SQLITE_OK {
            return Err(handle
                .last_error()
                .unwrap_or_else(|| SqliteError::from_code(status))
                .into());
        }
        Ok(())
    }
}

/// The arguments supplied to one invocation of a SQLite scalar function.
pub struct SqliteFunctionArgs<'r>(&'r [*mut ffi::sqlite3_value]);

impl SqliteFunctionArgs<'_> {
    /// Returns the number of arguments.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether there are no arguments.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Decode the argument at the given zero-based index using SQLite's usual conversions.
    /// Use `Option<T>` to distinguish SQL NULL from a value.
    pub fn get<'a, T: Decode<'a, Sqlite>>(&'a mut self, index: usize) -> Result<T, BoxDynError> {
        let &value = self
            .0
            .get(index)
            .ok_or_else(|| format!("SQLite function argument {index} out of bounds"))?;
        // SQLite keeps the value alive for the callback. The exclusive borrow
        // prevents another conversion from invalidating a borrowed string or blob.
        T::decode(unsafe { SqliteValueRef::borrowed(value) })
    }
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let _ = catch_unwind(|| unsafe { drop(Box::from_raw(data.cast::<Callback>())) });
}

unsafe extern "C" fn call(
    ctx: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    // SQLite supplies protected values valid on this thread for the duration of the call.
    // With zero arguments argv may be null.
    let result = catch_unwind(AssertUnwindSafe(|| unsafe {
        let callback = &*ffi::sqlite3_user_data(ctx).cast::<Callback>();
        let args = if argc == 0 {
            &[]
        } else {
            let len = usize::try_from(argc).expect("negative SQLite argument count");
            slice::from_raw_parts(argv, len)
        };
        match callback(&mut SqliteFunctionArgs(args)) {
            Ok(value) => set_result(ctx, value),
            Err(error) => {
                let message = error.to_string();
                if let Ok(len) = c_int::try_from(message.len()) {
                    ffi::sqlite3_result_error(ctx, message.as_ptr().cast(), len);
                } else {
                    ffi::sqlite3_result_error_toobig(ctx);
                }
            }
        }
    }));
    if result.is_err() {
        unsafe { ffi::sqlite3_result_error(ctx, c"SQLite scalar function panicked".as_ptr(), -1) };
    }
}

unsafe fn set_result(ctx: *mut ffi::sqlite3_context, value: SqliteArgumentValue) {
    use SqliteArgumentValue::*;

    // SQLITE_TRANSIENT copies text and blob bytes before their owners are dropped.
    let text = |s: &str| unsafe {
        ffi::sqlite3_result_text64(
            ctx,
            s.as_ptr().cast(),
            s.len() as u64,
            ffi::SQLITE_TRANSIENT(),
            u8::try_from(ffi::SQLITE_UTF8).expect("SQLITE_UTF8 fits in one byte"),
        );
    };
    unsafe {
        match value {
            Null => ffi::sqlite3_result_null(ctx),
            Int(n) => ffi::sqlite3_result_int(ctx, n),
            Int64(n) => ffi::sqlite3_result_int64(ctx, n),
            Double(n) => ffi::sqlite3_result_double(ctx, n),
            Text(s) => text(&s),
            TextSlice(s) => text(&s),
            Blob(b) => ffi::sqlite3_result_blob64(
                ctx,
                b.as_ptr().cast(),
                b.len() as u64,
                ffi::SQLITE_TRANSIENT(),
            ),
        }
    }
}
