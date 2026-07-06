use crate::options::{SqliteJournalMode, SqliteSynchronous};
use crate::{
    Sqlite, SqliteConnectOptions, SqliteQueryResult, SqliteRow, SqliteStatement, SqliteTypeInfo,
};

use sqlx_core::acquire::Acquire;
use sqlx_core::error::Error;
use sqlx_core::executor::{Execute, Executor};
use sqlx_core::pool::{MaybePoolConnection, Pool, PoolConnection, PoolOptions};
use sqlx_core::sql_str::SqlStr;
use sqlx_core::transaction::Transaction;
use sqlx_core::Either;

use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;

use std::fmt;

// ─── SqliteRwPoolOptions ───────────────────────────────────────────────────────

/// Builder for [`SqliteRwPool`].
///
/// Provides full control over both the reader and writer pools, including
/// independent [`SqliteConnectOptions`] and [`PoolOptions`] for each.
///
/// # Example
///
/// ```rust,no_run
/// # async fn example() -> sqlx::Result<()> {
/// use sqlx::sqlite::{SqliteRwPoolOptions, SqliteConnectOptions};
/// use sqlx::pool::PoolOptions;
/// use std::time::Duration;
///
/// let pool = SqliteRwPoolOptions::new()
///     .max_readers(4)
///     .writer_pool_options(
///         PoolOptions::new().acquire_timeout(Duration::from_secs(10))
///     )
///     .connect("sqlite://data.db").await?;
/// # Ok(())
/// # }
/// ```
pub struct SqliteRwPoolOptions {
    max_readers: Option<u32>,
    reader_connect_options: Option<SqliteConnectOptions>,
    writer_connect_options: Option<SqliteConnectOptions>,
    reader_pool_options: Option<PoolOptions<Sqlite>>,
    writer_pool_options: Option<PoolOptions<Sqlite>>,
    checkpoint_on_close: bool,
}

impl Default for SqliteRwPoolOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl SqliteRwPoolOptions {
    /// Create a new `SqliteRwPoolOptions` with sensible defaults.
    ///
    /// Defaults:
    /// - `max_readers`: number of available CPUs (or 4 if unavailable)
    /// - `checkpoint_on_close`: `true`
    pub fn new() -> Self {
        Self {
            max_readers: None,
            reader_connect_options: None,
            writer_connect_options: None,
            reader_pool_options: None,
            writer_pool_options: None,
            checkpoint_on_close: true,
        }
    }

    /// Set the maximum number of reader connections.
    ///
    /// Defaults to the number of available CPUs.
    pub fn max_readers(mut self, max: u32) -> Self {
        self.max_readers = Some(max);
        self
    }

    /// Override the [`SqliteConnectOptions`] used for reader connections.
    ///
    /// WAL journal mode and `read_only(true)` will still be applied on top.
    pub fn reader_connect_options(mut self, opts: SqliteConnectOptions) -> Self {
        self.reader_connect_options = Some(opts);
        self
    }

    /// Override the [`SqliteConnectOptions`] used for the writer connection.
    ///
    /// WAL journal mode and `synchronous(Normal)` will still be applied on top.
    pub fn writer_connect_options(mut self, opts: SqliteConnectOptions) -> Self {
        self.writer_connect_options = Some(opts);
        self
    }

    /// Override the [`PoolOptions`] used for the reader pool.
    ///
    /// `max_connections` will be overridden by [`max_readers`](Self::max_readers)
    /// if also set.
    pub fn reader_pool_options(mut self, opts: PoolOptions<Sqlite>) -> Self {
        self.reader_pool_options = Some(opts);
        self
    }

    /// Override the [`PoolOptions`] used for the writer pool.
    ///
    /// `max_connections` is always forced to 1 for the writer pool.
    pub fn writer_pool_options(mut self, opts: PoolOptions<Sqlite>) -> Self {
        self.writer_pool_options = Some(opts);
        self
    }

    /// Run `PRAGMA wal_checkpoint(PASSIVE)` on close.
    ///
    /// Enabled by default. This flushes as much WAL data as possible to the
    /// main database file without blocking.
    pub fn checkpoint_on_close(mut self, checkpoint: bool) -> Self {
        self.checkpoint_on_close = checkpoint;
        self
    }

    /// Create the pool by parsing a connection URL.
    pub async fn connect(self, url: &str) -> Result<SqliteRwPool, Error> {
        let options: SqliteConnectOptions = url.parse()?;
        self.connect_with(options).await
    }

    /// Create the pool from explicit [`SqliteConnectOptions`].
    ///
    /// The writer pool is created first to ensure WAL mode is established
    /// before any readers connect.
    pub async fn connect_with(
        self,
        base_options: SqliteConnectOptions,
    ) -> Result<SqliteRwPool, Error> {
        let num_cpus = std::thread::available_parallelism()
            .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
            .unwrap_or(4);

        // Configure writer: WAL mode + synchronous(Normal)
        let writer_opts = self
            .writer_connect_options
            .unwrap_or_else(|| base_options.clone());

        // Configure reader: read_only only.
        // WAL mode is NOT set here because the reader connection is opened with
        // SQLITE_OPEN_READONLY, and `PRAGMA journal_mode = wal` is a write operation
        // that would deadlock on a read-only connection. The writer already ensures
        // WAL mode is active on the database file; readers inherit it automatically.
        let reader_opts = self
            .reader_connect_options
            .unwrap_or(base_options)
            .read_only(true);

        // Writer pool: always exactly 1 connection
        let writer_pool_opts = self
            .writer_pool_options
            .unwrap_or_default()
            .max_connections(1);

        // Reader pool: configurable, defaults to num_cpus
        let max_readers = self.max_readers.unwrap_or(num_cpus);
        let reader_pool_opts = self
            .reader_pool_options
            .unwrap_or_default()
            .max_connections(max_readers);

        // Create writer pool FIRST — establishes WAL mode on the database file
        let write_pool = writer_pool_opts.connect_with(writer_opts).await?;

        // Then create reader pool
        let read_pool = reader_pool_opts.connect_with(reader_opts).await?;

        Ok(SqliteRwPool {
            read_pool,
            write_pool,
            checkpoint_on_close: self.checkpoint_on_close,
        })
    }
}

// ─── SqliteRwPool ──────────────────────────────────────────────────────────────

/// A single-writer, multi-reader connection pool for SQLite.
///
/// SQLite only allows one writer at a time. When multiple connections compete
/// for the write lock, you get busy timeouts and performance degradation.
/// `SqliteRwPool` solves this by maintaining:
///
/// - A **writer pool** with a single connection for all write operations
/// - A **reader pool** with multiple read-only connections for queries
///
/// Use [`reader()`](SqliteRwPool::reader) and [`writer()`](SqliteRwPool::writer)
/// to explicitly route queries to the appropriate pool. The [`Acquire`] trait
/// and the [`Executor`] impl always use the writer pool as a safe default.
///
/// # WAL Mode
///
/// This pool requires and automatically configures
/// [WAL mode](https://www.sqlite.org/wal.html), which allows concurrent
/// readers alongside a single writer.
///
/// # Important
///
/// You must call [`close()`](SqliteRwPool::close) explicitly for the WAL
/// checkpoint to run. Dropping the pool without calling `close()` will skip
/// the checkpoint, even though `checkpoint_on_close` is enabled by default.
/// The checkpoint uses `PASSIVE` mode, which flushes as much WAL data as
/// possible without blocking.
///
/// # Example
///
/// ```rust,no_run
/// # async fn example() -> sqlx::Result<()> {
/// use sqlx::sqlite::SqliteRwPool;
///
/// let pool = SqliteRwPool::connect("sqlite://data.db").await?;
///
/// // Reads go through the reader pool
/// let rows = sqlx::query("SELECT * FROM users")
///     .fetch_all(pool.reader()).await?;
///
/// // Writes go through the writer pool
/// sqlx::query("INSERT INTO users (name) VALUES (?)")
///     .bind("Alice")
///     .execute(pool.writer()).await?;
///
/// pool.close().await;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SqliteRwPool {
    read_pool: Pool<Sqlite>,
    write_pool: Pool<Sqlite>,
    checkpoint_on_close: bool,
}

impl SqliteRwPool {
    /// Create a pool with default options by parsing a connection URL.
    ///
    /// Equivalent to `SqliteRwPoolOptions::new().connect(url)`.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        SqliteRwPoolOptions::new().connect(url).await
    }

    /// Create a pool with default options from explicit connect options.
    ///
    /// Equivalent to `SqliteRwPoolOptions::new().connect_with(options)`.
    pub async fn connect_with(options: SqliteConnectOptions) -> Result<Self, Error> {
        SqliteRwPoolOptions::new().connect_with(options).await
    }

    /// Get a reference to the underlying reader pool.
    ///
    /// Use this to explicitly route read queries to the reader pool for
    /// concurrent read access.
    ///
    /// # Note
    ///
    /// Attempting to execute a write statement on a reader connection will
    /// return a `SQLITE_READONLY` error from SQLite.
    pub fn reader(&self) -> &Pool<Sqlite> {
        &self.read_pool
    }

    /// Get a reference to the underlying writer pool.
    pub fn writer(&self) -> &Pool<Sqlite> {
        &self.write_pool
    }

    /// Acquire a read-only connection from the reader pool.
    pub fn acquire_reader(
        &self,
    ) -> impl std::future::Future<Output = Result<PoolConnection<Sqlite>, Error>> + 'static {
        self.read_pool.acquire()
    }

    /// Acquire a writable connection from the writer pool.
    pub fn acquire_writer(
        &self,
    ) -> impl std::future::Future<Output = Result<PoolConnection<Sqlite>, Error>> + 'static {
        self.write_pool.acquire()
    }

    /// Start a transaction on the writer pool.
    pub async fn begin(&self) -> Result<Transaction<'static, Sqlite>, Error> {
        let conn = self.write_pool.acquire().await?;
        Transaction::begin(MaybePoolConnection::PoolConnection(conn), None).await
    }

    /// Start a transaction on the writer pool with a custom `BEGIN` statement.
    pub async fn begin_with(
        &self,
        statement: impl sqlx_core::sql_str::SqlSafeStr,
    ) -> Result<Transaction<'static, Sqlite>, Error> {
        let conn = self.write_pool.acquire().await?;
        Transaction::begin(
            MaybePoolConnection::PoolConnection(conn),
            Some(statement.into_sql_str()),
        )
        .await
    }

    /// Shut down the pool.
    ///
    /// If `checkpoint_on_close` is enabled (the default), closes all reader
    /// connections first, then runs `PRAGMA wal_checkpoint(PASSIVE)` on the
    /// writer to flush as much WAL data as possible to the main database file.
    pub async fn close(&self) {
        // Close readers first so the checkpoint isn't blocked by active readers.
        self.read_pool.close().await;

        if self.checkpoint_on_close && !self.write_pool.is_closed() {
            if let Ok(mut conn) = self.write_pool.acquire().await {
                // Best-effort WAL checkpoint
                let _ = Executor::execute(&mut *conn, "PRAGMA wal_checkpoint(PASSIVE)").await;
            }
        }

        self.write_pool.close().await;
    }

    /// Returns `true` if either pool has been closed.
    pub fn is_closed(&self) -> bool {
        self.write_pool.is_closed() || self.read_pool.is_closed()
    }

    /// Returns the number of active reader connections (including idle).
    pub fn num_readers(&self) -> u32 {
        self.read_pool.size()
    }

    /// Returns the number of idle reader connections.
    pub fn num_idle_readers(&self) -> usize {
        self.read_pool.num_idle()
    }

    /// Returns the number of active writer connections (including idle).
    pub fn num_writers(&self) -> u32 {
        self.write_pool.size()
    }

    /// Returns the number of idle writer connections.
    pub fn num_idle_writers(&self) -> usize {
        self.write_pool.num_idle()
    }
}

impl fmt::Debug for SqliteRwPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteRwPool")
            .field("read_pool", &self.read_pool)
            .field("write_pool", &self.write_pool)
            .field("checkpoint_on_close", &self.checkpoint_on_close)
            .finish()
    }
}

// ─── Executor impl ─────────────────────────────────────────────────────────────

/// All queries executed directly on `&SqliteRwPool` go to the writer pool.
/// Use [`SqliteRwPool::reader()`] to explicitly route reads to the reader pool.
impl<'p> Executor<'p> for &SqliteRwPool {
    type Database = Sqlite;

    fn fetch_many<'e, 'q, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<Either<SqliteQueryResult, SqliteRow>, Error>>
    where
        'p: 'e,
        E: Execute<'q, Sqlite>,
        'q: 'e,
        E: 'q,
    {
        (&self.write_pool).fetch_many(query)
    }

    fn fetch_optional<'e, 'q, E>(self, query: E) -> BoxFuture<'e, Result<Option<SqliteRow>, Error>>
    where
        'p: 'e,
        E: Execute<'q, Sqlite>,
        'q: 'e,
        E: 'q,
    {
        (&self.write_pool).fetch_optional(query)
    }

    fn prepare_with<'e>(
        self,
        sql: SqlStr,
        parameters: &'e [SqliteTypeInfo],
    ) -> BoxFuture<'e, Result<SqliteStatement, Error>>
    where
        'p: 'e,
    {
        (&self.write_pool).prepare_with(sql, parameters)
    }

    #[doc(hidden)]
    #[cfg(feature = "offline")]
    fn describe<'e>(
        self,
        sql: SqlStr,
    ) -> BoxFuture<'e, Result<sqlx_core::describe::Describe<Sqlite>, Error>>
    where
        'p: 'e,
    {
        (&self.write_pool).describe(sql)
    }
}

// ─── Acquire impl ──────────────────────────────────────────────────────────────

impl<'a> Acquire<'a> for &SqliteRwPool {
    type Database = Sqlite;
    type Connection = PoolConnection<Sqlite>;

    /// Always acquires from the writer pool.
    ///
    /// This is the safe default because code using `acquire()` may need to
    /// write, and [`sqlx::migrate!().run()`] uses `Acquire` internally.
    /// Use [`SqliteRwPool::acquire_reader()`] for explicit read-only access.
    fn acquire(self) -> BoxFuture<'static, Result<Self::Connection, Error>> {
        Box::pin(self.write_pool.acquire())
    }

    /// Begins a transaction on the writer pool.
    fn begin(self) -> BoxFuture<'static, Result<Transaction<'a, Sqlite>, Error>> {
        let pool = self.write_pool.clone();

        Box::pin(async move {
            let conn = pool.acquire().await?;
            Transaction::begin(MaybePoolConnection::PoolConnection(conn), None).await
        })
    }
}
