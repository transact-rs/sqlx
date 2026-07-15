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

    #[tracing::instrument(
        target = "sqlx::transaction",
        name = "mysql.transaction.begin",
        skip_all,
        fields(db.system = "mysql", depth = conn.inner.transaction_depth),
        level = "debug",
    )]
    async fn begin(conn: &mut MySqlConnection, statement: Option<SqlStr>) -> Result<(), Error> {
        let depth = conn.inner.transaction_depth;

        let statement = match statement {
            // custom `BEGIN` statements are not allowed if we're already in a transaction
            // (we need to issue a `SAVEPOINT` instead)
            Some(_) if depth > 0 => return Err(Error::InvalidSavePointStatement),
            Some(statement) => statement,
            None => begin_ansi_transaction_sql(depth),
        };
        conn.execute(statement).await?;
        if !conn.in_transaction() {
            return Err(Error::BeginFailed);
        }
        conn.inner.transaction_depth += 1;

        tracing::debug!("transaction/savepoint opened");

        Ok(())
    }

    #[tracing::instrument(
        target = "sqlx::transaction",
        name = "mysql.transaction.commit",
        skip_all,
        fields(db.system = "mysql", depth = conn.inner.transaction_depth),
        level = "debug",
    )]
    async fn commit(conn: &mut MySqlConnection) -> Result<(), Error> {
        let depth = conn.inner.transaction_depth;

        if depth > 0 {
            conn.execute(commit_ansi_transaction_sql(depth)).await?;
            conn.inner.transaction_depth = depth - 1;
        }

        Ok(())
    }

    #[tracing::instrument(
        target = "sqlx::transaction",
        name = "mysql.transaction.rollback",
        skip_all,
        fields(db.system = "mysql", depth = conn.inner.transaction_depth),
        level = "debug",
    )]
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
            tracing::debug!(
                target: "sqlx::transaction",
                depth,
                "queueing implicit rollback for unfinished transaction/savepoint on drop",
            );
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
