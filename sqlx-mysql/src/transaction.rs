use sqlx_core::sql_str::SqlStr;

use crate::connection::Waiting;
use crate::error::Error;
use crate::executor::Executor;
use crate::protocol::text::Query;
use crate::{MySql, MySqlConnection};

pub(crate) use sqlx_core::transaction::*;

/// Implementation of [`TransactionManager`] for MySQL.
pub struct MySqlTransactionManager;

impl TransactionManager for MySqlTransactionManager {
    type Database = MySql;

    async fn begin(conn: &mut MySqlConnection, statement: Option<SqlStr>) -> Result<(), Error> {
        let depth = conn.inner.transaction_depth;

        let statement = match statement {
            // custom `BEGIN` statements are not allowed if we're already in a transaction
            // (we need to issue a `SAVEPOINT` instead)
            Some(_) if depth > 0 => return Err(Error::InvalidSavePointStatement),
            Some(statement) => statement,
            None => begin_ansi_transaction_sql(depth),
        };
        // Claim the depth before the round trip, not after. `start_rollback` is a no-op while
        // the depth is zero, so a future cancelled during the await below would otherwise leave
        // the server in a transaction with nothing queued to end it, and `Pool` would hand that
        // connection to the next borrower. Unwound just below if the BEGIN did not take.
        conn.inner.transaction_depth += 1;
        let rollback = Rollback::new(conn);
        rollback.conn.execute(statement).await?;
        if !rollback.conn.in_transaction() {
            rollback.conn.inner.transaction_depth -= 1;
            return Err(Error::BeginFailed);
        }
        rollback.defuse();

        Ok(())
    }

    async fn commit(conn: &mut MySqlConnection) -> Result<(), Error> {
        let depth = conn.inner.transaction_depth;

        if depth > 0 {
            conn.execute(commit_ansi_transaction_sql(depth)).await?;
            conn.inner.transaction_depth = depth - 1;
        }

        Ok(())
    }

    async fn rollback(conn: &mut MySqlConnection) -> Result<(), Error> {
        let depth = conn.inner.transaction_depth;

        if depth > 0 {
            conn.execute(rollback_ansi_transaction_sql(depth)).await?;
            conn.inner.transaction_depth = depth - 1;
        }

        Ok(())
    }

    fn start_rollback(conn: &mut MySqlConnection) {
        let depth = conn.inner.transaction_depth;

        if depth > 0 {
            conn.inner.stream.waiting.push_back(Waiting::Result);
            conn.inner.stream.sequence_id = 0;
            conn.inner
                .stream
                .write_packet(Query(rollback_ansi_transaction_sql(depth).as_str()))
                .expect("BUG: unexpected error queueing ROLLBACK");

            conn.inner.transaction_depth = depth - 1;
        }
    }

    fn get_transaction_depth(conn: &MySqlConnection) -> usize {
        conn.inner.transaction_depth
    }
}

struct Rollback<'c> {
    conn: &'c mut MySqlConnection,
    defuse: bool,
}

impl Drop for Rollback<'_> {
    fn drop(&mut self) {
        if !self.defuse {
            MySqlTransactionManager::start_rollback(self.conn)
        }
    }
}

impl<'c> Rollback<'c> {
    fn new(conn: &'c mut MySqlConnection) -> Self {
        Self {
            conn,
            defuse: false,
        }
    }
    fn defuse(mut self) {
        self.defuse = true;
    }
}
