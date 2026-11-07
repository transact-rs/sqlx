use std::fmt::{self, Display, Formatter};

use futures_channel::oneshot;
use sqlx_core::error::Error;
use sqlx_core::executor::Execute;
use sqlx_core::sql_str::SqlStr;
use sqlx_core::Either;

use crate::connection::{execute, ConnectionState};
use crate::{Sqlite, SqliteArguments, SqliteConnection, SqliteQueryResult, SqliteRow};

/// The output of one query in a batch executed by [`SqliteConnection::execute_batch()`].
#[derive(Debug, Default)]
pub struct SqliteBatchResult {
    result: SqliteQueryResult,
    rows: Vec<SqliteRow>,
}

impl SqliteBatchResult {
    /// The number of rows changed by the query.
    pub fn rows_affected(&self) -> u64 {
        self.result.rows_affected()
    }

    /// The rowid of the last row inserted on the connection after the query finished.
    pub fn last_insert_rowid(&self) -> i64 {
        self.result.last_insert_rowid()
    }

    /// The rows returned by the query, including `RETURNING` rows.
    pub fn rows(&self) -> &[SqliteRow] {
        &self.rows
    }

    /// Returns the rows returned by the query, including `RETURNING` rows.
    pub fn into_rows(self) -> Vec<SqliteRow> {
        self.rows
    }

    /// Returns the query result and the rows returned by the query.
    pub fn into_parts(self) -> (SqliteQueryResult, Vec<SqliteRow>) {
        (self.result, self.rows)
    }
}

/// The error returned by [`SqliteConnection::execute_batch()`].
///
/// Holds the index of the query that failed. Queries before it have already run and are not
/// rolled back; queries after it did not run.
#[derive(Debug)]
pub struct SqliteBatchError {
    index: usize,
    error: Error,
}

impl SqliteBatchError {
    pub(crate) fn new(index: usize, error: Error) -> Self {
        Self { index, error }
    }

    /// The index of the query that failed, in the order the queries were given.
    pub fn index(&self) -> usize {
        self.index
    }

    /// The error returned by the failed query.
    pub fn error(&self) -> &Error {
        &self.error
    }

    /// Returns the error returned by the failed query.
    pub fn into_error(self) -> Error {
        self.error
    }
}

impl Display for SqliteBatchError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "query {} of batch failed: {}", self.index, self.error)
    }
}

impl std::error::Error for SqliteBatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl From<SqliteBatchError> for Error {
    fn from(error: SqliteBatchError) -> Self {
        error.error
    }
}

/// A query in a batch, as sent to the worker.
pub(crate) struct BatchQuery {
    sql: SqlStr,
    arguments: Option<SqliteArguments>,
    persistent: bool,
}

impl SqliteConnection {
    /// Execute a sequence of queries, in order, with a single round trip to the worker thread.
    ///
    /// Each query is executed as by [`Executor::fetch_many()`][sqlx_core::executor::Executor::fetch_many]:
    /// it keeps its own bind parameters and is prepared and cached according to
    /// [`Query::persistent()`][sqlx_core::query::Query::persistent], so repeating the same SQL
    /// with different arguments reuses one prepared statement.
    ///
    /// Use this to cut the per-query overhead of sending a command to the worker thread and
    /// waiting for its reply, which dominates the cost of many small queries, such as inserting
    /// rows one at a time inside a transaction.
    ///
    /// Returns one [`SqliteBatchResult`] per query, holding all of its rows.
    ///
    /// ```rust,no_run
    /// # async fn example(conn: &mut sqlx::SqliteConnection) -> Result<(), sqlx::Error> {
    /// use sqlx::Connection;
    ///
    /// let mut tx = conn.begin().await?;
    ///
    /// let results = tx
    ///     .execute_batch((1..=3).map(|id| {
    ///         sqlx::query("INSERT INTO item (id) VALUES (?) RETURNING id").bind(id)
    ///     }))
    ///     .await?;
    /// assert_eq!(results.len(), 3);
    ///
    /// tx.commit().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    /// Stops at the first query that fails, and returns its index with the error. Queries
    /// before it are not rolled back; run the batch in a transaction for atomicity.
    ///
    /// # Cancellation
    /// If the returned future is dropped after the batch has been sent to the worker thread,
    /// the query being executed runs to completion, and the rest are not executed.
    pub async fn execute_batch<'q, E>(
        &mut self,
        queries: impl IntoIterator<Item = E>,
    ) -> Result<Vec<SqliteBatchResult>, SqliteBatchError>
    where
        E: Execute<'q, Sqlite>,
    {
        let queries = queries
            .into_iter()
            .enumerate()
            .map(|(index, mut query)| {
                let arguments = query
                    .take_arguments()
                    .map_err(|e| SqliteBatchError::new(index, Error::Encode(e)))?;
                let persistent = query.persistent() && arguments.is_some();
                Ok(BatchQuery {
                    sql: query.sql(),
                    arguments,
                    persistent,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        if queries.is_empty() {
            return Ok(Vec::new());
        }

        self.worker.execute_batch(queries).await
    }
}

/// Runs a batch on the worker thread, stopping early if the caller has gone away.
pub(crate) fn run(
    conn: &mut ConnectionState,
    queries: Vec<BatchQuery>,
    tx: &oneshot::Sender<Result<Vec<SqliteBatchResult>, SqliteBatchError>>,
) -> Option<Result<Vec<SqliteBatchResult>, SqliteBatchError>> {
    let mut results = Vec::with_capacity(queries.len());

    for (index, query) in queries.into_iter().enumerate() {
        if tx.is_canceled() {
            return None;
        }

        let fail = |error| SqliteBatchError::new(index, error);

        let iter = match execute::iter(conn, query.sql, query.arguments, query.persistent) {
            Ok(iter) => iter,
            Err(error) => return Some(Err(fail(error))),
        };

        let mut output = SqliteBatchResult::default();

        for res in iter {
            match res {
                Ok(Either::Left(result)) => output.result.extend([result]),
                Ok(Either::Right(row)) => output.rows.push(row),
                Err(error) => return Some(Err(fail(error))),
            }
        }

        results.push(output);
    }

    Some(Ok(results))
}
