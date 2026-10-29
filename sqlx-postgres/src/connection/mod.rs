use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::sync::Arc;

use crate::HashMap;

use crate::common::StatementCache;
use crate::error::Error;
use crate::ext::ustr::UStr;
use crate::io::StatementId;
use crate::message::{
    BackendMessageFormat, Close, Query, ReadyForQuery, ReceivedMessage, Terminate,
    TransactionStatus,
};
use crate::statement::PgStatementMetadata;
use crate::transaction::Transaction;
use crate::types::Oid;
use crate::{PgConnectOptions, PgTypeInfo, Postgres};

pub(crate) use sqlx_core::connection::*;
use sqlx_core::sql_str::SqlSafeStr;

pub use self::stream::PgStream;

#[cfg(feature = "offline")]
mod describe;
mod establish;
mod executor;
mod resolve;
mod sasl;
mod stream;
mod tls;

/// A connection to a PostgreSQL database.
///
/// See [`PgConnectOptions`] for connection URL reference.
pub struct PgConnection {
    pub(crate) inner: Box<PgConnectionInner>,
}

pub struct PgConnectionInner {
    // underlying TCP or UDS stream,
    // wrapped in a potentially TLS stream,
    // wrapped in a buffered stream
    pub(crate) stream: PgStream,

    // process id of this backend
    // used to send cancel requests
    #[allow(dead_code)]
    process_id: u32,

    // secret key of this backend
    // used to send cancel requests
    #[allow(dead_code)]
    secret_key: u32,

    // sequence of statement IDs for use in preparing statements
    // in PostgreSQL, the statement is prepared to a user-supplied identifier
    next_statement_id: StatementId,

    // cache statement by query string to the id and columns
    cache_statement: StatementCache<(StatementId, Arc<PgStatementMetadata>)>,

    // cache user-defined types by id <-> info
    cache_type_info: HashMap<Oid, PgTypeInfo>,
    cache_type_oid: HashMap<UStr, Oid>,
    cache_elem_type_to_array: HashMap<Oid, Oid>,
    cache_table_data: HashMap<Oid, TableData>,

    // number of ReadyForQuery messages that we are currently expecting
    pub(crate) pending_ready_for_query_count: usize,

    // current transaction status
    transaction_status: TransactionStatus,
    pub(crate) transaction_depth: usize,

    log_settings: LogSettings,
}

pub(crate) struct TableData {
    table_name: Arc<str>,
    /// Attribute number -> name.
    columns: BTreeMap<i16, Arc<str>>,
}

impl PgConnection {
    /// the version number of the server in `libpq` format
    pub fn server_version_num(&self) -> Option<u32> {
        self.inner.stream.server_version_num
    }

    // will return when the connection is ready for another query
    pub(crate) async fn wait_until_ready(&mut self) -> Result<(), Error> {
        if !self.inner.stream.write_buffer_mut().is_empty() {
            self.inner.stream.flush().await?;
        }

        while self.inner.pending_ready_for_query_count > 0 {
            let message = self.inner.stream.recv().await?;

            if let BackendMessageFormat::ReadyForQuery = message.format {
                self.handle_ready_for_query(message)?;
            }
        }

        Ok(())
    }

    async fn recv_ready_for_query(&mut self) -> Result<(), Error> {
        let r: ReadyForQuery = self.inner.stream.recv_expect().await?;

        self.inner.pending_ready_for_query_count -= 1;
        self.inner.transaction_status = r.transaction_status;

        Ok(())
    }

    #[inline(always)]
    fn handle_ready_for_query(&mut self, message: ReceivedMessage) -> Result<(), Error> {
        self.inner.pending_ready_for_query_count = self
            .inner
            .pending_ready_for_query_count
            .checked_sub(1)
            .ok_or_else(|| err_protocol!("received more ReadyForQuery messages than expected"))?;

        self.inner.transaction_status = message.decode::<ReadyForQuery>()?.transaction_status;

        Ok(())
    }

    /// Queue a simple query (not prepared) to execute the next time this connection is used.
    ///
    /// Used for rolling back transactions and releasing advisory locks.
    #[inline(always)]
    pub(crate) fn queue_simple_query(&mut self, query: &str) -> Result<(), Error> {
        self.inner.stream.write_msg(Query(query))?;
        self.inner.pending_ready_for_query_count += 1;

        Ok(())
    }

    pub(crate) fn in_transaction(&self) -> bool {
        match self.inner.transaction_status {
            TransactionStatus::Transaction => true,
            TransactionStatus::Error | TransactionStatus::Idle => false,
        }
    }

    /// The transaction status the server reported in the most recent
    /// [`ReadyForQuery`][crate::message::ReadyForQuery] message.
    ///
    /// This is the *server's* view of the session, which is not always the
    /// client's: [`Connection::is_in_transaction`] reports the client-side
    /// transaction depth, and that depth can be zero while the server is
    /// inside a block. A future cancelled while `BEGIN` is in flight, or a
    /// statement that fails inside a block opened by a multi-statement
    /// [`raw_sql`][crate::raw_sql] query, both leave the session in
    /// [`TransactionStatus::Transaction`] or [`TransactionStatus::Error`]
    /// with a depth of zero.
    ///
    /// Reading this costs no round trip: the status byte rides along on every
    /// `ReadyForQuery`, so it is already in memory.
    ///
    /// ```rust,no_run
    /// # use sqlx_postgres::{PgConnection, TransactionStatus};
    /// # fn example(conn: &PgConnection) {
    /// if conn.transaction_status() != TransactionStatus::Idle {
    ///     // the session is inside a transaction block, open or failed
    /// }
    /// # }
    /// ```
    pub fn transaction_status(&self) -> TransactionStatus {
        self.inner.transaction_status
    }

    pub(crate) async fn invalidate_cached_statement(
        &mut self,
        sql: &str,
        backend: bool,
    ) -> Result<(), Error> {
        self.wait_until_ready().await?;

        let Some((statement_id, _)) = self.inner.cache_statement.remove(sql) else {
            return Ok(());
        };

        if backend {
            self.inner
                .stream
                .write_msg(Close::Statement(statement_id))?;
            self.write_sync();
            self.inner.stream.flush().await?;

            self.wait_for_close_complete(1).await?;
            self.recv_ready_for_query().await?;
        }

        Ok(())
    }
}

impl Debug for PgConnection {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgConnection").finish()
    }
}

impl Connection for PgConnection {
    type Database = Postgres;

    type Options = PgConnectOptions;

    async fn close(mut self) -> Result<(), Error> {
        // The normal, graceful termination procedure is that the frontend sends a Terminate
        // message and immediately closes the connection.

        // On receipt of this message, the backend closes the
        // connection and terminates.
        self.inner.stream.send(Terminate).await?;
        self.inner.stream.shutdown().await?;

        Ok(())
    }

    async fn close_hard(mut self) -> Result<(), Error> {
        self.inner.stream.shutdown().await?;

        Ok(())
    }

    async fn ping(&mut self) -> Result<(), Error> {
        // Users were complaining about this showing up in query statistics on the server.
        // By sending a comment we avoid an error if the connection was in the middle of a rowset
        // self.execute("/* SQLx ping */").map_ok(|_| ()).boxed()

        // The simplest call-and-response that's possible.
        self.write_sync();
        self.wait_until_ready().await?;

        // `wait_until_ready` has just refreshed `transaction_status` from the server's own
        // `ReadyForQuery`, so this is free -- and it is the only view that catches a session
        // left inside a block with a client-side depth of zero. `Pool` pings on release, which
        // makes this the point where such a connection is cleaned up instead of being handed
        // to the next borrower. See `transaction_status` for how the depth comes to disagree.
        if self.inner.transaction_depth == 0
            && self.inner.transaction_status != TransactionStatus::Idle
        {
            // The depth guard above matters: `ping` is public and a caller may well use it
            // inside a transaction they are deliberately holding. A non-zero depth means the
            // client knows about the block and owns its lifetime, so it is left alone; only a
            // block the client has no record of is ended here. That also means there is never
            // a savepoint to restore to, hence a plain `ROLLBACK`.
            self.queue_simple_query("ROLLBACK")?;
            self.inner.stream.flush().await?;
            self.wait_until_ready().await?;
        }

        Ok(())
    }

    fn begin(
        &mut self,
    ) -> impl Future<Output = Result<Transaction<'_, Self::Database>, Error>> + Send + '_ {
        Transaction::begin(self, None)
    }

    fn begin_with(
        &mut self,
        statement: impl SqlSafeStr,
    ) -> impl Future<Output = Result<Transaction<'_, Self::Database>, Error>> + Send + '_
    where
        Self: Sized,
    {
        Transaction::begin(self, Some(statement.into_sql_str()))
    }

    fn cached_statements_size(&self) -> usize {
        self.inner.cache_statement.len()
    }

    async fn clear_cached_statements(&mut self) -> Result<(), Error> {
        self.inner.cache_type_oid.clear();

        let mut cleared = 0_usize;

        self.wait_until_ready().await?;

        while let Some((id, _)) = self.inner.cache_statement.remove_lru() {
            self.inner.stream.write_msg(Close::Statement(id))?;
            cleared += 1;
        }

        if cleared > 0 {
            self.write_sync();
            self.inner.stream.flush().await?;

            self.wait_for_close_complete(cleared).await?;
            self.recv_ready_for_query().await?;
        }

        Ok(())
    }

    fn shrink_buffers(&mut self) {
        self.inner.stream.shrink_buffers();
    }

    #[doc(hidden)]
    fn flush(&mut self) -> impl Future<Output = Result<(), Error>> + Send + '_ {
        self.wait_until_ready()
    }

    #[doc(hidden)]
    fn should_flush(&self) -> bool {
        !self.inner.stream.write_buffer().is_empty()
    }
}

// Implement `AsMut<Self>` so that `PgConnection` can be wrapped in
// a `PgAdvisoryLockGuard`.
//
// See: https://github.com/launchbadge/sqlx/issues/2520
impl AsMut<PgConnection> for PgConnection {
    fn as_mut(&mut self) -> &mut PgConnection {
        self
    }
}
