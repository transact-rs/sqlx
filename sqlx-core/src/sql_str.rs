use std::borrow::{Borrow, Cow};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// A SQL string that is safe to execute on a database connection.
///
/// A "safe" SQL string is one that is unlikely to contain a [SQL injection vulnerability][injection].
///
/// In practice, this means a string type that is unlikely to contain dynamic data or user input.
///
/// `&'static str` is the only string type that satisfies the requirements of this trait
/// (ignoring [`String::leak()`] which has niche use-cases) and so is the only string type that
/// natively implements this trait by default.
///
/// For other string types, use [`AssertSqlSafe`] to assert this property.
/// This is the only intended way to pass an owned `String` to [`query()`] and its related functions
/// as well as [`raw_sql()`].
///
/// The maintainers of SQLx take no responsibility for any data leaks or loss resulting from misuse
/// of this API.
///
/// ### Motivation
/// This is designed to act as a speed bump against naively using `format!()` to add dynamic data
/// or user input to a query, which is a classic vector for SQL injection as SQLx does not
/// provide any sort of escaping or sanitization (which would have to be specially implemented
/// for each database flavor/locale).
///
/// The recommended way to incorporate dynamic data or user input in a query is to use
/// bind parameters, which requires the query to execute as a prepared statement.
/// See [`query()`] for details.
///
/// This trait and [`AssertSqlSafe`] are intentionally analogous to
/// [`std::panic::UnwindSafe`] and [`std::panic::AssertUnwindSafe`], respectively.
///
/// [injection]: https://en.wikipedia.org/wiki/SQL_injection
/// [`query()`]: crate::query::query
/// [`raw_sql()`]: crate::raw_sql::raw_sql
#[diagnostic::on_unimplemented(
    label = "dynamic SQL string",
    message = "dynamic SQL strings should be audited for possible injections",
    note = "prefer literal SQL strings with bind parameters or `QueryBuilder` to add dynamic data to a query.

To bypass this error, manually audit for potential injection vulnerabilities and wrap with `AssertSqlSafe()`.
For details, see the docs for `SqlSafeStr`.\n",
    note = "this trait is only implemented for `&'static str`, not all `&str` like the compiler error may suggest"
)]
pub trait SqlSafeStr {
    /// Convert `self` to a [`SqlStr`].
    fn into_sql_str(self) -> SqlStr;
}

impl SqlSafeStr for &'static str {
    #[inline]
    fn into_sql_str(self) -> SqlStr {
        SqlStr(Repr::Static(self))
    }
}

/// Assert that a query string is safe to execute on a database connection.
///
/// Using this API means that **you** have made sure that the string contents do not contain a
/// [SQL injection vulnerability][injection]. It means that, if the string was constructed
/// dynamically, and/or from user input, you have taken care to sanitize the input yourself.
/// SQLx does not provide any sort of sanitization; the design of SQLx prefers the use
/// of prepared statements for dynamic input.
///
/// The maintainers of SQLx take no responsibility for any data leaks or loss resulting from misuse
/// of this API. **Use at your own risk.**
///
/// Note that `&'static str` implements [`SqlSafeStr`] directly and so does not need to be wrapped
/// with this type.
///
/// [injection]: https://en.wikipedia.org/wiki/SQL_injection
pub struct AssertSqlSafe<T>(pub T);

/// Note: copies the string.
///
/// It is recommended to pass one of the supported owned string types instead.
impl SqlSafeStr for AssertSqlSafe<&str> {
    #[inline]
    fn into_sql_str(self) -> SqlStr {
        SqlStr(Repr::Arced(self.0.into()))
    }
}
impl SqlSafeStr for AssertSqlSafe<String> {
    #[inline]
    fn into_sql_str(self) -> SqlStr {
        SqlStr(Repr::Owned(self.0))
    }
}

impl SqlSafeStr for AssertSqlSafe<Box<str>> {
    #[inline]
    fn into_sql_str(self) -> SqlStr {
        SqlStr(Repr::Boxed(self.0))
    }
}

// Note: this is not implemented for `Rc<str>` because it would make `QueryString: !Send`.
impl SqlSafeStr for AssertSqlSafe<Arc<str>> {
    #[inline]
    fn into_sql_str(self) -> SqlStr {
        SqlStr(Repr::Arced(self.0))
    }
}

impl SqlSafeStr for AssertSqlSafe<Arc<String>> {
    #[inline]
    fn into_sql_str(self) -> SqlStr {
        SqlStr(Repr::ArcString(self.0))
    }
}

impl SqlSafeStr for AssertSqlSafe<Cow<'static, str>> {
    fn into_sql_str(self) -> SqlStr {
        match self.0 {
            Cow::Borrowed(str) => str.into_sql_str(),
            Cow::Owned(str) => AssertSqlSafe(str).into_sql_str(),
        }
    }
}

/// A SQL string that is ready to execute on a database connection.
///
/// This is essentially `Cow<'static, str>` but which can be constructed from additional types
/// without copying.
///
/// See [`SqlSafeStr`] for details.
#[derive(Debug)]
pub struct SqlStr(Repr);

#[derive(Debug)]
enum Repr {
    /// We need a variant to memoize when we already have a static string, so we don't copy it.
    Static(&'static str),
    /// Thanks to the new niche in `String`, this doesn't increase the size beyond 3 words.
    /// We essentially get all these variants for free.
    Owned(String),
    Boxed(Box<str>),
    Arced(Arc<str>),
    /// Allows for dynamic shared ownership with `query_builder`.
    ArcString(Arc<String>),
}

impl Clone for SqlStr {
    fn clone(&self) -> Self {
        Self(match &self.0 {
            Repr::Static(s) => Repr::Static(s),
            Repr::Arced(s) => Repr::Arced(s.clone()),
            // If `.clone()` gets called once, assume it might get called again.
            _ => Repr::Arced(self.as_str().into()),
        })
    }
}

impl SqlSafeStr for SqlStr {
    #[inline]
    fn into_sql_str(self) -> SqlStr {
        self
    }
}

impl SqlStr {
    /// Borrow the inner query string.
    #[inline]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Static(s) => s,
            Repr::Owned(s) => s,
            Repr::Boxed(s) => s,
            Repr::Arced(s) => s,
            Repr::ArcString(s) => s,
        }
    }

    pub const fn from_static(sql: &'static str) -> Self {
        SqlStr(Repr::Static(sql))
    }
}

impl AsRef<str> for SqlStr {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for SqlStr {
    #[inline]
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl<T> PartialEq<T> for SqlStr
where
    T: AsRef<str>,
{
    fn eq(&self, other: &T) -> bool {
        self.as_str() == other.as_ref()
    }
}

impl Eq for SqlStr {}

impl Hash for SqlStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}
