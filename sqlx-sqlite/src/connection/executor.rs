use crate::{
    Sqlite, SqliteConnection, SqliteQueryResult, SqliteRow, SqliteStatement, SqliteTypeInfo,
};
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_util::{stream, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use sqlx_core::error::Error;
use sqlx_core::executor::{Execute, Executor};
use sqlx_core::sql_str::SqlStr;
use sqlx_core::Either;
use std::{future, pin::pin};

impl<'c> Executor<'c> for &'c mut SqliteConnection {
    type Database = Sqlite;

    fn fetch_many<'e, 'q, E>(
        self,
        mut query: E,
    ) -> BoxStream<'e, Result<Either<SqliteQueryResult, SqliteRow>, Error>>
    where
        'c: 'e,
        E: Execute<'q, Self::Database>,
        'q: 'e,
        E: 'q,
    {
        let arguments = match query.take_arguments().map_err(Error::Encode) {
            Ok(arguments) => arguments,
            Err(error) => return stream::once(future::ready(Err(error))).boxed(),
        };
        let persistent = query.persistent() && arguments.is_some();
        let sql = query.sql();

        Box::pin(
            self.worker
                .execute(sql, arguments, self.row_channel_size, persistent, None)
                .map_ok(flume::Receiver::into_stream)
                .try_flatten_stream(),
        )
    }

    fn fetch_optional<'e, 'q, E>(
        self,
        mut query: E,
    ) -> BoxFuture<'e, Result<Option<SqliteRow>, Error>>
    where
        'c: 'e,
        E: Execute<'q, Self::Database>,
        'q: 'e,
        E: 'q,
    {
        let arguments = match query.take_arguments().map_err(Error::Encode) {
            Ok(arguments) => arguments,
            Err(error) => return future::ready(Err(error)).boxed(),
        };
        let persistent = query.persistent() && arguments.is_some();

        Box::pin(async move {
            let sql = query.sql();
            let mut stream = pin!(self
                .worker
                .execute(sql, arguments, self.row_channel_size, persistent, Some(1))
                .map_ok(flume::Receiver::into_stream)
                .try_flatten_stream());

            while let Some(res) = stream.try_next().await? {
                if let Either::Right(row) = res {
                    return Ok(Some(row));
                }
            }

            Ok(None)
        })
    }

    fn prepare_with<'e>(
        self,
        sql: SqlStr,
        _parameters: &[SqliteTypeInfo],
    ) -> BoxFuture<'e, Result<SqliteStatement, Error>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            let statement = self.worker.prepare(sql).await?;

            Ok(statement)
        })
    }

    #[doc(hidden)]
    #[cfg(feature = "offline")]
    fn describe<'e>(
        self,
        sql: SqlStr,
    ) -> BoxFuture<'e, Result<sqlx_core::describe::Describe<Sqlite>, Error>>
    where
        'c: 'e,
    {
        Box::pin(async move { self.worker.describe(sql).await })
    }
}
