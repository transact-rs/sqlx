use crate::{
    Either, Sqlite, SqliteArgumentValue, SqliteArguments, SqliteColumn, SqliteConnectOptions,
    SqliteConnection, SqliteQueryResult, SqliteRow, SqliteTransactionManager, SqliteTypeInfo,
};
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_util::{FutureExt, StreamExt, TryFutureExt, TryStreamExt};

use sqlx_core::any::{
    AnyArguments, AnyColumn, AnyConnectOptions, AnyConnectionBackend, AnyQueryResult, AnyRow,
    AnyStatement, AnyTypeInfo, AnyTypeInfoKind, AnyValueKind,
};
use sqlx_core::sql_str::SqlStr;

use crate::arguments::SqliteArgumentsBuffer;
use crate::type_info::DataType;
use sqlx_core::connection::{ConnectOptions, Connection};
use sqlx_core::database::Database;
use sqlx_core::executor::Executor;
use sqlx_core::transaction::TransactionManager;
use std::pin::pin;
use std::sync::Arc;

sqlx_core::declare_driver_with_optional_migrate!(DRIVER = Sqlite);

impl AnyConnectionBackend for SqliteConnection {
    fn name(&self) -> &str {
        <Sqlite as Database>::NAME
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, sqlx_core::Result<()>> {
        Connection::close(*self).boxed()
    }

    fn close_hard(self: Box<Self>) -> BoxFuture<'static, sqlx_core::Result<()>> {
        Connection::close_hard(*self).boxed()
    }

    fn ping(&mut self) -> BoxFuture<'_, sqlx_core::Result<()>> {
        Connection::ping(self).boxed()
    }

    fn begin(&mut self, statement: Option<SqlStr>) -> BoxFuture<'_, sqlx_core::Result<()>> {
        SqliteTransactionManager::begin(self, statement).boxed()
    }

    fn commit(&mut self) -> BoxFuture<'_, sqlx_core::Result<()>> {
        SqliteTransactionManager::commit(self).boxed()
    }

    fn rollback(&mut self) -> BoxFuture<'_, sqlx_core::Result<()>> {
        SqliteTransactionManager::rollback(self).boxed()
    }

    fn start_rollback(&mut self) {
        SqliteTransactionManager::start_rollback(self)
    }

    fn get_transaction_depth(&self) -> usize {
        SqliteTransactionManager::get_transaction_depth(self)
    }

    fn shrink_buffers(&mut self) {
        // NO-OP.
    }

    fn flush(&mut self) -> BoxFuture<'_, sqlx_core::Result<()>> {
        Connection::flush(self).boxed()
    }

    fn should_flush(&self) -> bool {
        Connection::should_flush(self)
    }

    #[cfg(feature = "migrate")]
    fn as_migrate(
        &mut self,
    ) -> sqlx_core::Result<&mut (dyn sqlx_core::migrate::Migrate + Send + 'static)> {
        Ok(self)
    }

    fn fetch_many(
        &mut self,
        query: SqlStr,
        persistent: bool,
        arguments: Option<AnyArguments>,
    ) -> BoxStream<'_, sqlx_core::Result<Either<AnyQueryResult, AnyRow>>> {
        let persistent = persistent && arguments.is_some();
        let args = arguments.map(map_arguments);

        Box::pin(
            self.worker
                .execute(query, args, self.row_channel_size, persistent, None)
                .map_ok(flume::Receiver::into_stream)
                .try_flatten_stream()
                .map(
                    move |res: sqlx_core::Result<Either<SqliteQueryResult, SqliteRow>>| match res? {
                        Either::Left(result) => Ok(Either::Left(map_result(result))),
                        Either::Right(row) => Ok(Either::Right(AnyRow::try_from(&row)?)),
                    },
                ),
        )
    }

    fn fetch_optional(
        &mut self,
        query: SqlStr,
        persistent: bool,
        arguments: Option<AnyArguments>,
    ) -> BoxFuture<'_, sqlx_core::Result<Option<AnyRow>>> {
        let persistent = persistent && arguments.is_some();
        let args = arguments.map(map_arguments);

        Box::pin(async move {
            let mut stream = pin!(
                self.worker
                    .execute(query, args, self.row_channel_size, persistent, Some(1))
                    .map_ok(flume::Receiver::into_stream)
                    .await?
            );

            if let Some(Either::Right(row)) = stream.try_next().await? {
                return Ok(Some(AnyRow::try_from(&row)?));
            }

            Ok(None)
        })
    }

    fn prepare_with<'c, 'q: 'c>(
        &'c mut self,
        sql: SqlStr,
        _parameters: &[AnyTypeInfo],
    ) -> BoxFuture<'c, sqlx_core::Result<AnyStatement>> {
        Box::pin(async move {
            let statement = Executor::prepare_with(self, sql, &[]).await?;
            let column_names = statement.column_names.clone();
            AnyStatement::try_from_statement(statement, column_names)
        })
    }

    #[cfg(feature = "offline")]
    fn describe(
        &mut self,
        sql: SqlStr,
    ) -> BoxFuture<'_, sqlx_core::Result<sqlx_core::describe::Describe<sqlx_core::any::Any>>> {
        Box::pin(async move { Executor::describe(self, sql).await?.try_into_any() })
    }
}

impl<'a> TryFrom<&'a SqliteTypeInfo> for AnyTypeInfo {
    type Error = sqlx_core::Error;

    fn try_from(sqlite_type: &'a SqliteTypeInfo) -> Result<Self, Self::Error> {
        Ok(AnyTypeInfo {
            kind: match &sqlite_type.0 {
                DataType::Null => AnyTypeInfoKind::Null,
                DataType::Int4 => AnyTypeInfoKind::Integer,
                DataType::Integer => AnyTypeInfoKind::BigInt,
                DataType::Float => AnyTypeInfoKind::Double,
                DataType::Blob => AnyTypeInfoKind::Blob,
                DataType::Text => AnyTypeInfoKind::Text,
                _ => {
                    return Err(sqlx_core::Error::AnyDriverError(
                        format!("Any driver does not support the SQLite type {sqlite_type:?}")
                            .into(),
                    ))
                }
            },
        })
    }
}

impl<'a> TryFrom<&'a SqliteColumn> for AnyColumn {
    type Error = sqlx_core::Error;

    fn try_from(col: &'a SqliteColumn) -> Result<Self, Self::Error> {
        let type_info =
            AnyTypeInfo::try_from(&col.type_info).map_err(|e| sqlx_core::Error::ColumnDecode {
                index: col.name.to_string(),
                source: e.into(),
            })?;

        Ok(AnyColumn {
            ordinal: col.ordinal,
            name: col.name.clone(),
            type_info,
        })
    }
}

impl<'a> TryFrom<&'a SqliteRow> for AnyRow {
    type Error = sqlx_core::Error;

    fn try_from(row: &'a SqliteRow) -> Result<Self, Self::Error> {
        AnyRow::map_from(row, row.column_names.clone())
    }
}

impl<'a> TryFrom<&'a AnyConnectOptions> for SqliteConnectOptions {
    type Error = sqlx_core::Error;

    fn try_from(opts: &'a AnyConnectOptions) -> Result<Self, Self::Error> {
        let mut opts_out = SqliteConnectOptions::from_url(&opts.database_url)?;
        opts_out.log_settings = opts.log_settings.clone();

        Ok(opts_out)
    }
}

// Infallible alternative to AnyArguments::convert_into()
fn map_arguments(args: AnyArguments) -> SqliteArguments {
    let values = args
        .values
        .0
        .into_iter()
        .map(|val| match val {
            AnyValueKind::Null(_) => SqliteArgumentValue::Null,
            AnyValueKind::Bool(b) => SqliteArgumentValue::Int(b as i32),
            AnyValueKind::SmallInt(i) => SqliteArgumentValue::Int(i as i32),
            AnyValueKind::Integer(i) => SqliteArgumentValue::Int(i),
            AnyValueKind::BigInt(i) => SqliteArgumentValue::Int64(i),
            AnyValueKind::Real(r) => SqliteArgumentValue::Double(r as f64),
            AnyValueKind::Double(d) => SqliteArgumentValue::Double(d),
            AnyValueKind::Text(t) => SqliteArgumentValue::Text(Arc::new(t.to_string())),
            AnyValueKind::Blob(b) => SqliteArgumentValue::Blob(Arc::new(b.to_vec())),
            // AnyValueKind is `#[non_exhaustive]` but we should have covered everything
            _ => unreachable!("BUG: missing mapping for {val:?}"),
        })
        .collect();

    SqliteArguments {
        values: SqliteArgumentsBuffer::new(values),
    }
}

fn map_result(res: SqliteQueryResult) -> AnyQueryResult {
    AnyQueryResult {
        rows_affected: res.rows_affected(),
        last_insert_id: None,
    }
}
