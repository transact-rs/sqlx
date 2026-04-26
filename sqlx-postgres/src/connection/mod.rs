use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::sync::Arc;

use crate::HashMap;
use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use pipe::Pipe;
use request::{IoRequest, MessageBuf};
use worker::Shared;

use crate::common::StatementCache;
use crate::error::Error;
use crate::ext::ustr::UStr;
use crate::io::StatementId;
use crate::message::{Close, FrontendMessage, Notification, Query, TransactionStatus};
use crate::statement::PgStatementMetadata;
use crate::transaction::Transaction;
use crate::types::Oid;
use crate::{PgConnectOptions, PgTypeInfo, Postgres};

pub(crate) use sqlx_core::connection::*;
use sqlx_core::sql_str::SqlSafeStr;

#[cfg(feature = "offline")]
mod describe;
mod establish;
mod executor;
mod pipe;
mod request;
mod resolve;
mod sasl;
mod tls;
mod worker;

/// A connection to a PostgreSQL database.
///
/// See [`PgConnectOptions`] for connection URL reference.
pub struct PgConnection {
    pub(crate) inner: Box<PgConnectionInner>,
}

pub struct PgConnectionInner {
    // channel to the background worker
    chan: UnboundedSender<IoRequest>,

    pub(crate) notifications: UnboundedReceiver<Notification>,

    // process id of this backend
    // used to send cancel requests
    #[allow(dead_code)]
    process_id: u32,

    // secret key of this backend
    // used to send cancel requests
    #[allow(dead_code)]
    secret_key: u32,

    pub(crate) server_version_num: Option<u32>,

    // sequence of statement IDs for use in preparing statements
    // in PostgreSQL, the statement is prepared to a user-supplied identifier
    next_statement_id: StatementId,

    // cache statement by query string to the id and columns
    cache_statement: StatementCache<(StatementId, Arc<PgStatementMetadata>)>,

    // cache user-defined types by id <-> info
    cache_type_info: HashMap<Oid, PgTypeInfo>,
    cache_type_oid: HashMap<UStr, Oid>,
    cache_elem_type_to_array: HashMap<Oid, Oid>,
    cache_table_to_column_names: HashMap<Oid, TableColumns>,

    pub(crate) transaction_depth: usize,

    log_settings: LogSettings,

    shared: Shared,
}

pub(crate) struct TableColumns {
    table_name: Arc<str>,
    /// Attribute number -> name.
    columns: BTreeMap<i16, Arc<str>>,
}

impl PgConnection {
    /// the version number of the server in `libpq` format
    pub fn server_version_num(&self) -> Option<u32> {
        self.inner.server_version_num
    }

    /// Queue a simple query (not prepared) to execute the next time this connection is used.
    ///
    /// Used for rolling back transactions and releasing advisory locks.
    #[inline(always)]
    pub(crate) fn queue_simple_query(&self, query: &str) -> Result<Pipe, Error> {
        self.pipe(|buf| buf.write_msg(Query(query)))
    }

    pub(crate) fn in_transaction(&self) -> bool {
        match self.inner.shared.get_transaction_status() {
            TransactionStatus::Transaction => true,
            TransactionStatus::Error | TransactionStatus::Idle => false,
        }
    }

    fn new(
        options: &PgConnectOptions,
        chan: UnboundedSender<IoRequest>,
        notifications: UnboundedReceiver<Notification>,
        shared: Shared,
    ) -> Self {
        Self {
            inner: Box::new(PgConnectionInner {
                chan,
                notifications,
                log_settings: options.log_settings.clone(),
                process_id: 0,
                secret_key: 0,
                next_statement_id: StatementId::NAMED_START,
                cache_statement: StatementCache::new(options.statement_cache_capacity),
                cache_type_info: HashMap::new(),
                cache_type_oid: HashMap::new(),
                cache_elem_type_to_array: HashMap::new(),
                cache_table_to_column_names: HashMap::new(),
                transaction_depth: 0,
                server_version_num: None,
                shared,
            }),
        }
    }

    fn create_request<F>(&self, callback: F) -> sqlx_core::Result<IoRequest>
    where
        F: FnOnce(&mut MessageBuf) -> sqlx_core::Result<()>,
    {
        let mut buffer = MessageBuf::new();
        (callback)(&mut buffer)?;
        Ok(buffer.finish())
    }

    fn send_request(&self, request: IoRequest) -> sqlx_core::Result<()> {
        self.inner
            .chan
            .unbounded_send(request)
            .map_err(|_| sqlx_core::Error::WorkerCrashed)
    }

    fn send_buf(&self, buf: MessageBuf) -> sqlx_core::Result<Pipe> {
        let mut req = buf.finish();
        let (tx, rx) = unbounded();
        req.chan = Some(tx);

        self.send_request(req)?;
        Ok(Pipe::new(rx))
    }

    pub(crate) fn pipe<F>(&self, callback: F) -> sqlx_core::Result<Pipe>
    where
        F: FnOnce(&mut MessageBuf) -> sqlx_core::Result<()>,
    {
        let mut req = self.create_request(callback)?;
        let (tx, rx) = unbounded();
        req.chan = Some(tx);

        self.send_request(req)?;
        Ok(Pipe::new(rx))
    }

    pub(crate) fn pipe_and_forget<T>(&self, value: T) -> sqlx_core::Result<()>
    where
        T: FrontendMessage,
    {
        let req = self.create_request(|buf| buf.write_msg(value))?;
        self.send_request(req)
    }

    pub(crate) async fn pipe_and_forget_async<F, R>(&self, callback: F) -> sqlx_core::Result<R>
    where
        F: AsyncFnOnce(&mut MessageBuf) -> sqlx_core::Result<R>,
    {
        let mut buffer = MessageBuf::new();
        let result = (callback)(&mut buffer).await?;
        let req = buffer.finish();
        self.send_request(req)?;

        Ok(result)
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

    async fn close(self) -> Result<(), Error> {
        // Closing the channel notifies the bg worker to start a graceful shutdown.
        self.inner.chan.close_channel();
        Ok(())
    }

    async fn close_hard(self) -> Result<(), Error> {
        self.close().await
    }

    async fn ping(&mut self) -> Result<(), Error> {
        // Users were complaining about this showing up in query statistics on the server.
        // By sending a comment we avoid an error if the connection was in the middle of a rowset
        // self.execute("/* SQLx ping */").map_ok(|_| ()).boxed()

        // The simplest call-and-response that's possible.
        let mut pipe = self.pipe(|buf| {
            buf.write_sync();
            Ok(())
        })?;
        pipe.recv_ready_for_query().await
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

        let mut buf = MessageBuf::new();

        while let Some((id, _)) = self.inner.cache_statement.remove_lru() {
            buf.write_msg(Close::Statement(id))?;
            cleared += 1;
        }

        if cleared > 0 {
            buf.write_sync();
            let mut pipe = self.send_buf(buf)?;

            pipe.wait_for_close_complete(cleared).await?;
            pipe.recv_ready_for_query().await?;
        }

        Ok(())
    }

    fn shrink_buffers(&mut self) {
        // No-op
    }

    #[doc(hidden)]
    async fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    #[doc(hidden)]
    fn should_flush(&self) -> bool {
        false
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
