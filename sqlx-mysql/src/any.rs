use crate::protocol::text::ColumnType;
use crate::{
    MySql, MySqlColumn, MySqlConnectOptions, MySqlConnection, MySqlQueryResult, MySqlRow,
    MySqlTransactionManager, MySqlTypeInfo,
};
use either::Either;
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_util::{stream, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use sqlx_core::any::{
    AnyArguments, AnyColumn, AnyConnectOptions, AnyConnectionBackend, AnyQueryResult, AnyRow,
    AnyStatement, AnyTypeInfo, AnyTypeInfoKind,
};
use sqlx_core::connection::Connection;
use sqlx_core::database::Database;
use sqlx_core::executor::Executor;
use sqlx_core::sql_str::SqlStr;
use sqlx_core::transaction::TransactionManager;
use std::{future, pin::pin};

sqlx_core::declare_driver_with_optional_migrate!(DRIVER = MySql);

impl AnyConnectionBackend for MySqlConnection {
    fn name(&self) -> &str {
        <MySql as Database>::NAME
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
        MySqlTransactionManager::begin(self, statement).boxed()
    }

    fn commit(&mut self) -> BoxFuture<'_, sqlx_core::Result<()>> {
        MySqlTransactionManager::commit(self).boxed()
    }

    fn rollback(&mut self) -> BoxFuture<'_, sqlx_core::Result<()>> {
        MySqlTransactionManager::rollback(self).boxed()
    }

    fn start_rollback(&mut self) {
        MySqlTransactionManager::start_rollback(self)
    }

    fn get_transaction_depth(&self) -> usize {
        MySqlTransactionManager::get_transaction_depth(self)
    }

    fn shrink_buffers(&mut self) {
        Connection::shrink_buffers(self);
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
        let arguments = match arguments.map(AnyArguments::convert_into).transpose() {
            Ok(arguments) => arguments,
            Err(error) => {
                return stream::once(future::ready(Err(sqlx_core::Error::Encode(error)))).boxed()
            }
        };

        Box::pin(
            self.run(query, arguments, persistent)
                .try_flatten_stream()
                .map(|res| {
                    Ok(match res? {
                        Either::Left(result) => Either::Left(map_result(result)),
                        Either::Right(row) => Either::Right(AnyRow::try_from(&row)?),
                    })
                }),
        )
    }

    fn fetch_optional(
        &mut self,
        query: SqlStr,
        persistent: bool,
        arguments: Option<AnyArguments>,
    ) -> BoxFuture<'_, sqlx_core::Result<Option<AnyRow>>> {
        let persistent = persistent && arguments.is_some();
        let arguments = arguments
            .map(AnyArguments::convert_into)
            .transpose()
            .map_err(sqlx_core::Error::Encode);

        Box::pin(async move {
            let arguments = arguments?;
            let mut stream = pin!(self.run(query, arguments, persistent).await?);

            while let Some(result) = stream.try_next().await? {
                if let Either::Right(row) = result {
                    return Ok(Some(AnyRow::try_from(&row)?));
                }
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
            let column_names = statement.metadata.column_names.clone();
            AnyStatement::try_from_statement(statement, column_names)
        })
    }

    #[cfg(feature = "offline")]
    fn describe(
        &mut self,
        sql: SqlStr,
    ) -> BoxFuture<'_, sqlx_core::Result<sqlx_core::describe::Describe<sqlx_core::any::Any>>> {
        Box::pin(async move {
            let describe = Executor::describe(self, sql).await?;
            describe.try_into_any()
        })
    }
}

impl<'a> TryFrom<&'a MySqlTypeInfo> for AnyTypeInfo {
    type Error = sqlx_core::Error;

    fn try_from(type_info: &'a MySqlTypeInfo) -> Result<Self, Self::Error> {
        Ok(AnyTypeInfo {
            kind: match &type_info.r#type {
                ColumnType::Null => AnyTypeInfoKind::Null,
                ColumnType::Short => AnyTypeInfoKind::SmallInt,
                ColumnType::Long => AnyTypeInfoKind::Integer,
                ColumnType::LongLong => AnyTypeInfoKind::BigInt,
                ColumnType::Float => AnyTypeInfoKind::Real,
                ColumnType::Double => AnyTypeInfoKind::Double,
                ColumnType::Blob
                | ColumnType::TinyBlob
                | ColumnType::MediumBlob
                | ColumnType::LongBlob => AnyTypeInfoKind::Blob,
                ColumnType::String | ColumnType::VarString | ColumnType::VarChar => {
                    AnyTypeInfoKind::Text
                }
                #[cfg(feature = "json")]
                ColumnType::Json => AnyTypeInfoKind::Json,
                _ => {
                    return Err(sqlx_core::Error::AnyDriverError(
                        format!("Any driver does not support MySql type {type_info:?}").into(),
                    ))
                }
            },
        })
    }
}

impl<'a> TryFrom<&'a MySqlColumn> for AnyColumn {
    type Error = sqlx_core::Error;

    fn try_from(column: &'a MySqlColumn) -> Result<Self, Self::Error> {
        let type_info = AnyTypeInfo::try_from(&column.type_info)?;

        Ok(AnyColumn {
            ordinal: column.ordinal,
            name: column.name.clone(),
            type_info,
        })
    }
}

impl<'a> TryFrom<&'a MySqlRow> for AnyRow {
    type Error = sqlx_core::Error;

    fn try_from(row: &'a MySqlRow) -> Result<Self, Self::Error> {
        AnyRow::map_from(row, row.column_names.clone())
    }
}

impl<'a> TryFrom<&'a AnyConnectOptions> for MySqlConnectOptions {
    type Error = sqlx_core::Error;

    fn try_from(any_opts: &'a AnyConnectOptions) -> Result<Self, Self::Error> {
        let mut opts = Self::parse_from_url(&any_opts.database_url)?;
        opts.log_settings = any_opts.log_settings.clone();
        Ok(opts)
    }
}

fn map_result(result: MySqlQueryResult) -> AnyQueryResult {
    AnyQueryResult {
        rows_affected: result.rows_affected,
        // Don't expect this to be a problem
        #[allow(clippy::cast_possible_wrap)]
        last_insert_id: Some(result.last_insert_id as i64),
    }
}
