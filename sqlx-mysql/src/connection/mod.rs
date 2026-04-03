use std::fmt::{self, Debug, Formatter};
use std::future::Future;

pub(crate) use sqlx_core::connection::*;
use sqlx_core::sql_str::SqlSafeStr;
pub(crate) use stream::{MySqlStream, Waiting};

use crate::collation::Collation;
use crate::common::StatementCache;
use crate::error::Error;
use crate::protocol::response::Status;
use crate::protocol::statement::StmtClose;
use crate::protocol::text::{Ping, Quit};
use crate::statement::MySqlStatementMetadata;
use crate::transaction::Transaction;
use crate::{MySql, MySqlConnectOptions};

mod auth;
mod compression;
mod establish;
mod executor;
mod stream;
mod tls;

const MAX_PACKET_SIZE: u32 = 1024;

/// The charset parameter sent in the `Protocol::HandshakeResponse41` packet.
///
/// This becomes the default if `set_names = false`,
/// and also ensures that any error messages returned before `SET NAMES` are encoded correctly.
#[allow(clippy::cast_possible_truncation)]
const INITIAL_CHARSET: u8 = Collation::UTF8MB4_GENERAL_CI.0 as u8;

/// A connection to a MySQL database.
pub struct MySqlConnection {
    pub(crate) inner: Box<MySqlConnectionInner>,
}

pub(crate) struct MySqlConnectionInner {
    // underlying TCP stream,
    // wrapped in a potentially TLS stream,
    // wrapped in a buffered stream
    pub(crate) stream: MySqlStream,

    // transaction status
    pub(crate) transaction_depth: usize,
    status_flags: Status,

    // cache by query string to the statement id and metadata
    cache_statement: StatementCache<(u32, MySqlStatementMetadata)>,

    log_settings: LogSettings,
}

impl MySqlConnection {
    pub(crate) fn in_transaction(&self) -> bool {
        self.inner
            .status_flags
            .intersects(Status::SERVER_STATUS_IN_TRANS)
    }
}

impl Debug for MySqlConnection {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("MySqlConnection").finish()
    }
}

impl Connection for MySqlConnection {
    type Database = MySql;

    type Options = MySqlConnectOptions;

    async fn close(mut self) -> Result<(), Error> {
        self.inner.stream.send_packet(Quit).await?;
        self.inner.stream.shutdown().await?;

        Ok(())
    }

    async fn close_hard(mut self) -> Result<(), Error> {
        self.inner.stream.shutdown().await?;
        Ok(())
    }

    async fn ping(&mut self) -> Result<(), Error> {
        self.inner.stream.wait_until_ready().await?;
        self.inner.stream.send_packet(Ping).await?;
        self.inner.stream.recv_ok().await?;

        Ok(())
    }

    #[doc(hidden)]
    fn flush(&mut self) -> impl Future<Output = Result<(), Error>> + Send + '_ {
        self.inner.stream.wait_until_ready()
    }

    fn cached_statements_size(&self) -> usize {
        self.inner.cache_statement.len()
    }

    async fn clear_cached_statements(&mut self) -> Result<(), Error> {
        while let Some((statement_id, _)) = self.inner.cache_statement.remove_lru() {
            self.inner
                .stream
                .send_packet(StmtClose {
                    statement: statement_id,
                })
                .await?;
        }

        Ok(())
    }

    #[doc(hidden)]
    fn should_flush(&self) -> bool {
        !self.inner.stream.write_buffer().is_empty()
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

    fn shrink_buffers(&mut self) {
        self.inner.stream.shrink_buffers();
    }
}
