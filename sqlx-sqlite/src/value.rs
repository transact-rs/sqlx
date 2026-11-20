use std::borrow::Cow;
use std::cell::OnceCell;
use std::ptr::NonNull;
use std::slice;
use std::str;

use libsqlite3_sys::{
    sqlite3_value, sqlite3_value_blob, sqlite3_value_bytes, sqlite3_value_double,
    sqlite3_value_dup, sqlite3_value_free, sqlite3_value_int64, sqlite3_value_type,
};
use sqlx_core::type_info::TypeInfo;
pub(crate) use sqlx_core::value::{Value, ValueRef};

use crate::type_info::DataType;
use crate::{Sqlite, SqliteError, SqliteTypeInfo};

/// An owned handle to a [`sqlite3_value`].
///
/// # Note: Decoding is Stateful
/// The [`sqlite3_value` interface][value-methods] reserves the right to be stateful:
///
/// > Other interfaces might change the datatype for an sqlite3_value object.
/// > For example, if the datatype is initially SQLITE_INTEGER and sqlite3_value_text(V) is called
/// > to extract a text value for that integer, then subsequent calls to sqlite3_value_type(V)
/// > might return SQLITE_TEXT. Whether or not a persistent internal datatype conversion occurs is
/// > undefined and may change from one release of SQLite to the next.
///
/// Thus, this type is `!Sync` and [`SqliteValueRef`] is `!Send` and `!Sync` to prevent data races.
///
/// Additionally, this statefulness means that the return values of `sqlite3_value_bytes()` and
/// `sqlite3_value_blob()` could be invalidated by later calls to other `sqlite3_value*` methods.
///
/// To prevent undefined behavior from accessing dangling pointers, this type (and any
/// [`SqliteValueRef`] instances created from it) remembers when it was used to decode a
/// borrowed `&[u8]` or `&str` and returns an error if it is used to decode any other type.
///
/// To bypass this error, you must prove that no outstanding borrows exist.
///
/// This may be done in one of a few ways:
/// * If you hold mutable access, call [`Self::reset_borrow()`] which resets the borrowed state.
/// * If you have an immutable reference, call [`Self::clone()`] to get a new instance
///   with no outstanding borrows.
/// * If you hold a [`SqliteValueRef`], call [`SqliteValueRef::to_owned()`]
///   to get a new `SqliteValue` with no outstanding borrows.
///
/// This is *only* necessary if using the same `SqliteValue` or [`SqliteValueRef`] to decode
/// multiple different types. The vast majority of use-cases employing once-through decoding
/// should not have to worry about this.
///
/// [`sqlite3_value`]: https://www.sqlite.org/c3ref/value.html
/// [value-methods]: https://www.sqlite.org/c3ref/value_blob.html
pub struct SqliteValue(ValueHandle);

/// A borrowed reference to a [`sqlite3_value`].
///
/// Semantically, this behaves as a reference to [`SqliteValue`].
///
/// # Note: Decoding is Stateful
/// See [`SqliteValue`] for details.
pub struct SqliteValueRef<'r>(Cow<'r, ValueHandle>);

impl SqliteValue {
    // SAFETY: The sqlite3_value must be non-null and SQLite must not free it. It will be freed on drop.
    pub(crate) unsafe fn dup(
        value: *mut sqlite3_value,
        column_type: Option<SqliteTypeInfo>,
    ) -> Self {
        debug_assert!(!value.is_null());
        let handle = ValueHandle::try_dup_of(value, column_type)
            .expect("SQLite failed to allocate memory for duplicated value");
        Self(handle)
    }

    /// Prove that there are no outstanding borrows of this instance.
    ///
    /// Call this after decoding a borrowed `&[u8]` or `&str`
    /// to reset the internal borrowed state and allow decoding of other types.
    pub fn reset_borrow(&mut self) {
        self.0.reset_blob_borrow();
    }

    /// Call [`sqlite3_value_dup()`] to create a new instance of this type.
    ///
    /// Returns an error if the call returns a null pointer, indicating that
    /// SQLite was unable to allocate the additional memory required.
    ///
    /// Non-panicking version of [`Self::clone()`].
    ///
    /// [`sqlite3_value_dup()`]: https://www.sqlite.org/c3ref/value_dup.html
    pub fn try_clone(&self) -> Result<Self, SqliteError> {
        self.0.try_dup().map(Self)
    }
}

impl Clone for SqliteValue {
    /// Call [`sqlite3_value_dup()`] to create a new instance of this type.
    ///
    /// # Panics
    /// If [`sqlite3_value_dup()`] returns a null pointer, indicating an out-of-memory condition.
    ///
    /// See [`Self::try_clone()`] for a non-panicking version.
    ///
    /// [`sqlite3_value_dup()`]: https://www.sqlite.org/c3ref/value_dup.html
    fn clone(&self) -> Self {
        self.try_clone().expect("failed to clone `SqliteValue`")
    }
}

impl Value for SqliteValue {
    type Database = Sqlite;

    fn as_ref(&self) -> SqliteValueRef<'_> {
        SqliteValueRef::value(self)
    }

    fn type_info(&self) -> Cow<'_, SqliteTypeInfo> {
        Cow::Owned(self.0.type_info())
    }

    fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

impl<'r> SqliteValueRef<'r> {
    /// Attempt to duplicate the internal `sqlite3_value` with [`sqlite3_value_dup()`].
    ///
    /// Returns an error if the call returns a null pointer, indicating that
    /// SQLite was unable to allocate the additional memory required.
    ///
    /// Non-panicking version of [`Self::try_to_owned()`].
    ///
    /// [`sqlite3_value_dup()`]: https://www.sqlite.org/c3ref/value_dup.html
    pub fn try_to_owned(&self) -> Result<SqliteValue, SqliteError> {
        self.0.try_dup().map(SqliteValue)
    }

    pub(crate) fn value(value: &'r SqliteValue) -> Self {
        Self(Cow::Borrowed(&value.0))
    }

    /// # Safety
    /// The supplied sqlite3_value must not be null and SQLite must free it.
    /// It will not be freed on drop.
    /// The lifetime on this struct should tie it to whatever scope it's valid for before SQLite frees it.
    pub(crate) unsafe fn borrowed(value: *mut sqlite3_value) -> Self {
        debug_assert!(!value.is_null());
        let handle = ValueHandle::temporary(NonNull::new_unchecked(value));
        Self(Cow::Owned(handle))
    }

    // NOTE: `int()` is deliberately omitted because it will silently truncate a wider value,
    // which is likely to cause bugs:
    // https://github.com/launchbadge/sqlx/issues/3179
    // (Similar bug in Postgres): https://github.com/launchbadge/sqlx/issues/3161
    pub(super) fn int64(&self) -> Result<i64, BorrowedBlobError> {
        self.0.int64()
    }

    pub(super) fn double(&self) -> Result<f64, BorrowedBlobError> {
        self.0.double()
    }

    pub(super) fn blob_borrowed(&self) -> &'r [u8] {
        // SAFETY: lifetime is matched to `'r`
        unsafe { self.0.blob_borrowed() }
    }

    pub(super) fn with_temp_blob<R>(&self, op: impl FnOnce(&[u8]) -> R) -> R {
        self.0.with_blob(op)
    }

    pub(super) fn blob_owned(&self) -> Vec<u8> {
        self.with_temp_blob(|blob| blob.to_vec())
    }

    pub(super) fn text_borrowed(&self) -> Result<&'r str, str::Utf8Error> {
        // SAFETY: lifetime is matched to `'r`
        unsafe { self.0.text_borrowed() }
    }

    pub(super) fn with_temp_text<R>(
        &self,
        op: impl FnOnce(&str) -> R,
    ) -> Result<R, str::Utf8Error> {
        self.0.with_blob(|blob| str::from_utf8(blob).map(op))
    }

    pub(super) fn text_owned(&self) -> Result<String, str::Utf8Error> {
        self.with_temp_text(|text| text.to_string())
    }
}

impl<'r> ValueRef<'r> for SqliteValueRef<'r> {
    type Database = Sqlite;

    /// Attempt to duplicate the internal `sqlite3_value` with [`sqlite3_value_dup()`].
    ///
    /// # Panics
    /// If [`sqlite3_value_dup()`] returns a null pointer, indicating an out-of-memory condition.
    ///
    /// See [`Self::try_to_owned()`] for a non-panicking version.
    ///
    /// [`sqlite3_value_dup()`]: https://www.sqlite.org/c3ref/value_dup.html
    fn to_owned(&self) -> SqliteValue {
        SqliteValue(
            self.0
                .try_dup()
                .expect("failed to convert SqliteValueRef to owned SqliteValue"),
        )
    }

    fn type_info(&self) -> Cow<'_, SqliteTypeInfo> {
        Cow::Owned(self.0.type_info())
    }

    fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

pub(crate) struct ValueHandle {
    value: NonNull<sqlite3_value>,
    column_type: Option<SqliteTypeInfo>,
    // Note: `std::cell` version
    borrowed_blob: OnceCell<Blob>,
    free_on_drop: bool,
}

struct Blob {
    ptr: *const u8,
    len: usize,
}

#[derive(Debug, thiserror::Error)]
#[error("given `SqliteValue` was previously decoded as BLOB or TEXT; `SqliteValue::reset_borrow()` must be called first")]
pub(crate) struct BorrowedBlobError;

// SAFE: only protected value objects are stored in SqliteValue
unsafe impl Send for ValueHandle {}

// SAFETY: the `sqlite3_value_*()` methods reserve the right to be stateful,
// which means method calls aren't thread-safe without mutual exclusion.
//
// impl !Sync for ValueHandle {}

impl ValueHandle {
    /// # Safety
    /// The `sqlite3_value` must be valid and SQLite must not free it. It will be freed on drop.
    unsafe fn try_dup_of(
        value: *mut sqlite3_value,
        column_type: Option<SqliteTypeInfo>,
    ) -> Result<Self, SqliteError> {
        // SAFETY: caller must ensure `value` is valid.
        let value =
            unsafe { NonNull::new(sqlite3_value_dup(value)).ok_or_else(SqliteError::nomem)? };

        Ok(Self {
            value,
            column_type,
            borrowed_blob: OnceCell::new(),
            free_on_drop: true,
        })
    }

    fn temporary(value: NonNull<sqlite3_value>) -> Self {
        Self {
            value,
            column_type: None,
            borrowed_blob: OnceCell::new(),
            free_on_drop: false,
        }
    }

    fn try_dup(&self) -> Result<Self, SqliteError> {
        // SAFETY: `value` is initialized
        unsafe { Self::try_dup_of(self.value.as_ptr(), self.column_type.clone()) }
    }

    fn value_type_info(&self) -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::from_code(unsafe {
            sqlite3_value_type(self.value.as_ptr())
        }))
    }

    fn type_info(&self) -> SqliteTypeInfo {
        let value_type = self.value_type_info();

        // Assume the actual value type is more accurate, if it's not NULL.
        match &self.column_type {
            Some(column_type) if value_type.is_null() => column_type.clone(),
            _ => value_type,
        }
    }

    fn int64(&self) -> Result<i64, BorrowedBlobError> {
        // SAFETY: we have to be certain the caller isn't still holding a borrow from `.blob_borrowed()`
        self.assert_blob_not_borrowed()?;

        Ok(unsafe { sqlite3_value_int64(self.value.as_ptr()) })
    }

    fn double(&self) -> Result<f64, BorrowedBlobError> {
        // SAFETY: we have to be certain the caller isn't still holding a borrow from `.blob_borrowed()`
        self.assert_blob_not_borrowed()?;

        Ok(unsafe { sqlite3_value_double(self.value.as_ptr()) })
    }

    fn is_null(&self) -> bool {
        self.value_type_info().is_null()
    }
}

impl Clone for ValueHandle {
    fn clone(&self) -> Self {
        self.try_dup().unwrap()
    }
}

impl Drop for ValueHandle {
    fn drop(&mut self) {
        if self.free_on_drop {
            unsafe {
                sqlite3_value_free(self.value.as_ptr());
            }
        }
    }
}

impl ValueHandle {
    fn assert_blob_not_borrowed(&self) -> Result<(), BorrowedBlobError> {
        if self.borrowed_blob.get().is_none() {
            Ok(())
        } else {
            Err(BorrowedBlobError)
        }
    }

    fn reset_blob_borrow(&mut self) {
        self.borrowed_blob.take();
    }

    fn get_blob(&self) -> Option<Blob> {
        if let Some(blob) = self.borrowed_blob.get() {
            return Some(Blob { ..*blob });
        }

        // SAFETY: calling `sqlite3_value_bytes` from multiple threads at once is a data race.
        let len = unsafe { sqlite3_value_bytes(self.value.as_ptr()) };

        // This likely means UB in SQLite itself or our usage of it;
        // signed integer overflow is UB in the C standard.
        let len = usize::try_from(len).unwrap_or_else(|_| {
            panic!("sqlite3_value_bytes() returned value out of range for usize: {len}")
        });

        if len == 0 {
            // empty blobs are NULL
            return None;
        }

        let ptr = unsafe { sqlite3_value_blob(self.value.as_ptr()) } as *const u8;
        debug_assert!(!ptr.is_null());

        Some(Blob { ptr, len })
    }

    fn with_blob<R>(&self, with_blob: impl FnOnce(&[u8]) -> R) -> R {
        let Some(blob) = self.get_blob() else {
            return with_blob(&[]);
        };

        // SAFETY: the slice cannot outlive the call
        with_blob(unsafe { blob.as_slice() })
    }

    /// # Safety
    /// Caller must ensure lifetime '`b` cannot outlive `self`.
    unsafe fn blob_borrowed<'a>(&self) -> &'a [u8] {
        let Some(blob) = self.get_blob() else {
            return &[];
        };

        // SAFETY: we need to store that the blob was borrowed
        // to prevent
        let blob = self.borrowed_blob.get_or_init(|| blob);

        unsafe { blob.as_slice() }
    }

    /// # Safety
    /// Caller must ensure lifetime '`b` cannot outlive `self`.
    unsafe fn text_borrowed<'b>(&self) -> Result<&'b str, str::Utf8Error> {
        let Some(blob) = self.get_blob() else {
            return Ok("");
        };

        // SAFETY: lifetime of `blob` will be tied to `'b`.
        let s = str::from_utf8(unsafe { blob.as_slice() })?;

        // We only store the borrow after we ensure the string is valid.
        self.borrowed_blob.set(blob).ok();

        Ok(s)
    }
}

impl Blob {
    /// # Safety
    /// `'a` must not outlive the `sqlite3_value` this blob came from.
    unsafe fn as_slice<'a>(&self) -> &'a [u8] {
        slice::from_raw_parts(self.ptr, self.len)
    }
}
