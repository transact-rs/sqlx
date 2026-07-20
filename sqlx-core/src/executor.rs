use crate::database::Database;
use crate::error::{BoxDynError, Error};
use crate::sql_str::{SqlSafeStr, SqlStr};

use either::Either;
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_util::{FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use std::{fmt::Debug, future};

/// A type that contains or can provide a database
/// connection to use for executing queries against the database.
///
/// No guarantees are provided that successive queries run on the same
/// physical database connection.
///
/// A [`Connection`](crate::connection::Connection) is an `Executor` that guarantees that
/// successive queries are ran on the same physical database connection.
///
/// Implemented for the following:
///
///  * [`&Pool`](super::pool::Pool)
///  * [`&mut Connection`](super::connection::Connection)
///
/// The [`Executor`] impls for [`Transaction`](crate::transaction::Transaction)
/// and [`PoolConnection`](crate::pool::PoolConnection) have been deleted because they
/// cannot exist in the new crate architecture without rewriting the Executor trait entirely.
/// To fix this breakage, simply add a dereference where an impl [`Executor`] is expected, as
/// they both dereference to the inner connection type which will still implement it:
/// * `&mut transaction` -> `&mut *transaction`
/// * `&mut connection` -> `&mut *connection`
///
/// # Note: Methods Not for General Use
/// This is a building-block trait that is exposed mainly for use as a trait bound.
///
/// Instead of calling the methods of this trait, use the free functions in the `sqlx` crate root:
/// * [`sqlx::query()`]: use for DML queries (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, etc.)
///     * uses and caches prepared statements internally per-connection
///     * may use argument placeholders (MySQL/MariaDB: `?`, Postgres/SQLite: `$1`, `$2`, etc.)
///     * one query per string
/// * [`sqlx::query_as()`], [`sqlx::query_scalar()`]: preferred for DML queries with type mapping
/// * [`sqlx::raw_sql()`]: for DDL (`CREATE`, `ALTER`, etc.), batch queries and administration functions
///     * accepts multiple queries separated by semicolons (`;`) in the same string
///     * never uses prepared statements
///
/// Consider also the `query!()` family of macros, which offer queries and type mapping checked
/// at compile-time.
///
/// [`sqlx::query()`]: crate::query::query
/// [`sqlx::query_as()`]: crate::query_as::query_as
/// [`sqlx::query_scalar()`]: crate::query_scalar::query_scalar
/// [`sqlx::raw_sql()`]: crate::raw_sql::raw_sql
pub trait Executor<'c>: Send + Debug + Sized {
    type Database: Database;

    /// Execute the query and return the total number of rows affected.
    fn execute<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<<Self::Database as Database>::QueryResult, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Self::Database>,
    {
        self.execute_many(query).try_collect().boxed()
    }

    /// Execute multiple queries and return the rows affected from each query, in a stream.
    fn execute_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<<Self::Database as Database>::QueryResult, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Self::Database>,
    {
        self.fetch_many(query)
            .try_filter_map(|step| async move {
                Ok(match step {
                    Either::Left(rows) => Some(rows),
                    Either::Right(_) => None,
                })
            })
            .boxed()
    }

    /// Execute the query and return the generated results as a stream.
    fn fetch<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<<Self::Database as Database>::Row, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Self::Database>,
    {
        self.fetch_many(query)
            .try_filter_map(|step| async move {
                Ok(match step {
                    Either::Left(_) => None,
                    Either::Right(row) => Some(row),
                })
            })
            .boxed()
    }

    /// Execute multiple queries and return the generated results as a stream
    /// from each query, in a stream.
    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<
        'e,
        Result<
            Either<<Self::Database as Database>::QueryResult, <Self::Database as Database>::Row>,
            Error,
        >,
    >
    where
        'c: 'e,
        E: 'q + Execute<'q, Self::Database>;

    /// Execute the query and return all the generated results, collected into a [`Vec`].
    fn fetch_all<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Vec<<Self::Database as Database>::Row>, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Self::Database>,
    {
        self.fetch(query).try_collect().boxed()
    }

    /// Execute the query and returns exactly one row.
    fn fetch_one<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<<Self::Database as Database>::Row, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Self::Database>,
    {
        self.fetch_optional(query)
            .and_then(|row| {
                future::ready(match row {
                    Some(row) => Ok(row),
                    None => Err(Error::RowNotFound),
                })
            })
            .boxed()
    }

    /// Execute the query and returns at most one row.
    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Option<<Self::Database as Database>::Row>, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Self::Database>;

    /// Prepare the SQL query to inspect the type information of its parameters
    /// and results.
    ///
    /// Be advised that when using the `query`, `query_as`, or `query_scalar` functions, the query
    /// is transparently prepared and executed.
    ///
    /// This explicit API is provided to allow access to the statement metadata available after
    /// it prepared but before the first row is returned.
    #[inline]
    fn prepare<'e>(
        self,
        query: SqlStr,
    ) -> BoxFuture<'e, Result<<Self::Database as Database>::Statement, Error>>
    where
        'c: 'e,
    {
        self.prepare_with(query, &[])
    }

    /// Prepare the SQL query, with parameter type information, to inspect the
    /// type information about its parameters and results.
    ///
    /// Only some database drivers (PostgreSQL, MSSQL) can take advantage of
    /// this extra information to influence parameter type inference.
    fn prepare_with<'e>(
        self,
        sql: SqlStr,
        parameters: &'e [<Self::Database as Database>::TypeInfo],
    ) -> BoxFuture<'e, Result<<Self::Database as Database>::Statement, Error>>
    where
        'c: 'e;

    /// Describe the SQL query and return type information about its parameters
    /// and results.
    ///
    /// This is used by compile-time verification in the query macros to
    /// power their type inference.
    #[doc(hidden)]
    #[cfg(feature = "offline")]
    fn describe<'e>(
        self,
        sql: SqlStr,
    ) -> BoxFuture<'e, Result<crate::describe::Describe<Self::Database>, Error>>
    where
        'c: 'e;
}

/// A type that may be executed against a database connection.
///
/// Implemented for the following:
///
///  * [`&str`](std::str)
///  * [`Query`](super::query::Query)
///
pub trait Execute<'q, DB: Database>: Send + Sized {
    /// Gets the SQL that will be executed.
    fn sql(self) -> SqlStr;

    /// Gets the previously cached statement, if available.
    fn statement(&self) -> Option<&DB::Statement>;

    /// Returns the arguments to be bound against the query string.
    ///
    /// Returning `Ok(None)` for `Arguments` indicates to use a "simple" query protocol and to not
    /// prepare the query. Returning `Ok(Some(Default::default()))` is an empty arguments object that
    /// will be prepared (and cached) before execution.
    ///
    /// Returns `Err` if encoding any of the arguments failed.
    fn take_arguments(&mut self) -> Result<Option<<DB as Database>::Arguments>, BoxDynError>;

    /// Returns `true` if the statement should be cached.
    fn persistent(&self) -> bool;
}

impl<DB: Database, T> Execute<'_, DB> for T
where
    T: SqlSafeStr + Send,
{
    #[inline]
    fn sql(self) -> SqlStr {
        self.into_sql_str()
    }

    #[inline]
    fn statement(&self) -> Option<&DB::Statement> {
        None
    }

    #[inline]
    fn take_arguments(&mut self) -> Result<Option<<DB as Database>::Arguments>, BoxDynError> {
        Ok(None)
    }

    #[inline]
    fn persistent(&self) -> bool {
        true
    }
}

impl<DB: Database, T> Execute<'_, DB> for (T, Option<<DB as Database>::Arguments>)
where
    T: SqlSafeStr + Send,
{
    #[inline]
    fn sql(self) -> SqlStr {
        self.0.into_sql_str()
    }

    #[inline]
    fn statement(&self) -> Option<&DB::Statement> {
        None
    }

    #[inline]
    fn take_arguments(&mut self) -> Result<Option<<DB as Database>::Arguments>, BoxDynError> {
        Ok(self.1.take())
    }

    #[inline]
    fn persistent(&self) -> bool {
        true
    }
}
