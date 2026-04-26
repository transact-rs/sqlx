use crate::error::Error;
use crate::executor::{Execute, Executor};
use crate::io::{PortalId, StatementId};
use crate::logger::QueryLogger;
use crate::message::{
    self, BackendMessageFormat, Bind, Close, CommandComplete, DataRow, ParameterDescription, Parse,
    ParseComplete, RowDescription,
};
use crate::statement::PgStatementMetadata;
use crate::{
    statement::PgStatement, PgArguments, PgConnection, PgQueryResult, PgRow, PgTypeInfo,
    PgValueFormat, Postgres,
};
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_core::Stream;
use futures_util::TryStreamExt;
use sqlx_core::arguments::Arguments;
use sqlx_core::sql_str::SqlStr;
use sqlx_core::Either;
use std::{pin::pin, sync::Arc};

use super::pipe::Pipe;

async fn prepare(
    conn: &mut PgConnection,
    sql: &str,
    parameters: &[PgTypeInfo],
    metadata: Option<Arc<PgStatementMetadata>>,
    persistent: bool,
    fetch_column_origin: bool,
) -> Result<(StatementId, Arc<PgStatementMetadata>), Error> {
    let id = if persistent {
        let id = conn.inner.next_statement_id;
        conn.inner.next_statement_id = id.next();
        id
    } else {
        StatementId::UNNAMED
    };

    // build a list of type OIDs to send to the database in the PARSE command
    // we have not yet started the query sequence, so we are *safe* to cleanly make
    // additional queries here to get any missing OIDs

    let mut param_types = Vec::with_capacity(parameters.len());

    for ty in parameters {
        param_types.push(conn.resolve_type_id(&ty.0).await?);
    }

    let mut pipe = conn.pipe(|buf| {
        // next we send the PARSE command to the server
        buf.write_msg(Parse {
            param_types: &param_types,
            query: sql,
            statement: id,
        })?;

        if metadata.is_none() {
            // get the statement columns and parameters
            buf.write_msg(message::Describe::Statement(id))?;
        }

        // we ask for the server to immediately send us the result of the PARSE command
        buf.write_sync();
        Ok(())
    })?;

    // indicates that the SQL query string is now successfully parsed and has semantic validity
    pipe.recv_expect::<ParseComplete>().await?;

    let metadata = if let Some(metadata) = metadata {
        // each SYNC produces one READY FOR QUERY
        pipe.recv_ready_for_query().await?;

        // we already have metadata
        metadata
    } else {
        let parameters = recv_desc_params(&mut pipe).await?;

        let rows = recv_desc_rows(&mut pipe).await?;

        // each SYNC produces one READY FOR QUERY
        pipe.recv_ready_for_query().await?;

        let parameters = conn.handle_parameter_description(parameters).await?;

        let (columns, column_names) = conn
            .handle_row_description(rows, true, fetch_column_origin)
            .await?;

        Arc::new(PgStatementMetadata {
            parameters,
            columns,
            column_names: Arc::new(column_names),
        })
    };

    Ok((id, metadata))
}

async fn recv_desc_params(pipe: &mut Pipe) -> Result<ParameterDescription, Error> {
    pipe.recv_expect().await
}

async fn recv_desc_rows(pipe: &mut Pipe) -> Result<Option<RowDescription>, Error> {
    let rows: Option<RowDescription> = match pipe.recv().await? {
        // describes the rows that will be returned when the statement is eventually executed
        message if message.format == BackendMessageFormat::RowDescription => {
            Some(message.decode()?)
        }

        // no data would be returned if this statement was executed
        message if message.format == BackendMessageFormat::NoData => None,

        message => {
            return Err(err_protocol!(
                "expecting RowDescription or NoData but received {:?}",
                message.format
            ));
        }
    };

    Ok(rows)
}

impl PgConnection {
    async fn get_or_prepare(
        &mut self,
        sql: &str,
        parameters: &[PgTypeInfo],
        persistent: bool,
        // optional metadata that was provided by the user, this means they are reusing
        // a statement object
        metadata: Option<Arc<PgStatementMetadata>>,
        fetch_column_origin: bool,
    ) -> Result<(StatementId, Arc<PgStatementMetadata>), Error> {
        if let Some(statement) = self.inner.cache_statement.get_mut(sql) {
            return Ok((*statement).clone());
        }

        let statement = prepare(
            self,
            sql,
            parameters,
            metadata,
            persistent,
            fetch_column_origin,
        )
        .await?;

        if persistent && self.inner.cache_statement.is_enabled() {
            if let Some((id, _)) = self.inner.cache_statement.insert(sql, statement.clone()) {
                let mut pipe = self.pipe(|buf| {
                    buf.write_msg(Close::Statement(id))?;
                    buf.write_sync();
                    Ok(())
                })?;

                pipe.wait_for_close_complete(1).await?;
                pipe.recv_ready_for_query().await?;
            }
        }

        Ok(statement)
    }

    pub(crate) async fn run<'e, 'c: 'e, 'q: 'e>(
        &'c mut self,
        query: SqlStr,
        arguments: Option<PgArguments>,
        persistent: bool,
        metadata_opt: Option<Arc<PgStatementMetadata>>,
    ) -> Result<impl Stream<Item = Result<Either<PgQueryResult, PgRow>, Error>> + 'e, Error> {
        let mut logger = QueryLogger::new(query, self.inner.log_settings.clone());
        let sql = logger.sql().as_str();

        let mut metadata: Arc<PgStatementMetadata>;
        let mut pipe: Pipe;

        let format = if let Some(mut arguments) = arguments {
            // Check this before we write anything to the stream.
            //
            // Note: Postgres actually interprets this value as unsigned,
            // making the max number of parameters 65535, not 32767
            // https://github.com/launchbadge/sqlx/issues/3464
            // https://www.postgresql.org/docs/current/limits.html
            let num_params = u16::try_from(arguments.len()).map_err(|_| {
                err_protocol!(
                    "PgConnection::run(): too many arguments for query: {}",
                    arguments.len()
                )
            })?;

            // prepare the statement if this our first time executing it
            // always return the statement ID here
            let (statement, metadata_) = self
                .get_or_prepare(sql, &arguments.types, persistent, metadata_opt, false)
                .await?;

            metadata = metadata_;

            // patch holes created during encoding
            arguments.apply_patches(self, &metadata.parameters).await?;

            pipe = self.pipe(|buf| {
                // bind to attach the arguments to the statement and create a portal
                buf.write_msg(Bind {
                    portal: PortalId::UNNAMED,
                    statement,
                    formats: &[PgValueFormat::Binary],
                    num_params,
                    params: &arguments.buffer,
                    result_formats: &[PgValueFormat::Binary],
                })?;

                // executes the portal up to the passed limit
                // the protocol-level limit acts nearly identically to the `LIMIT` in SQL
                buf.write_msg(message::Execute {
                    portal: PortalId::UNNAMED,
                    // Non-zero limits cause query plan pessimization by disabling parallel workers:
                    // https://github.com/launchbadge/sqlx/issues/3673
                    limit: 0,
                })?;
                // From https://www.postgresql.org/docs/current/protocol-flow.html:
                //
                // "An unnamed portal is destroyed at the end of the transaction, or as
                // soon as the next Bind statement specifying the unnamed portal as
                // destination is issued. (Note that a simple Query message also
                // destroys the unnamed portal."

                // we ask the database server to close the unnamed portal and free the associated resources
                // earlier - after the execution of the current query.
                buf.write_msg(Close::Portal(PortalId::UNNAMED))?;

                // finally, [Sync] asks postgres to process the messages that we sent and respond with
                // a [ReadyForQuery] message when it's completely done. Theoretically, we could send
                // dozens of queries before a [Sync] and postgres can handle that. Execution on the server
                // is still serial but it would reduce round-trips. Some kind of builder pattern that is
                // termed batching might suit this.
                buf.write_sync();
                Ok(())
            })?;

            // prepared statements are binary
            PgValueFormat::Binary
        } else {
            // Query will trigger a ReadyForQuery
            pipe = self.queue_simple_query(sql)?;

            // metadata starts out as "nothing"
            metadata = Arc::new(PgStatementMetadata::default());

            // and unprepared statements are text
            PgValueFormat::Text
        };

        Ok(try_stream! {
            loop {
                let message = pipe.recv().await?;

                match message.format {
                    BackendMessageFormat::BindComplete
                    | BackendMessageFormat::ParseComplete
                    | BackendMessageFormat::ParameterDescription
                    | BackendMessageFormat::NoData
                    // unnamed portal has been closed
                    | BackendMessageFormat::CloseComplete
                    => {
                        // harmless messages to ignore
                    }

                    // "Execute phase is always terminated by the appearance of
                    // exactly one of these messages: CommandComplete,
                    // EmptyQueryResponse (if the portal was created from an
                    // empty query string), ErrorResponse, or PortalSuspended"
                    BackendMessageFormat::CommandComplete => {
                        // a SQL command completed normally
                        let cc: CommandComplete = message.decode()?;

                        let rows_affected = cc.rows_affected();
                        logger.increase_rows_affected(rows_affected);
                        r#yield!(Either::Left(PgQueryResult {
                            rows_affected,
                        }));
                    }

                    BackendMessageFormat::EmptyQueryResponse => {
                        // empty query string passed to an unprepared execute
                    }

                    // Message::ErrorResponse is handled in self.stream.recv()

                    // incomplete query execution has finished
                    BackendMessageFormat::PortalSuspended => {}

                    BackendMessageFormat::RowDescription => {
                        // indicates that a *new* set of rows are about to be returned
                        let (columns, column_names) = self
                            .handle_row_description(Some(message.decode()?), false, false)
                            .await?;

                        metadata = Arc::new(PgStatementMetadata {
                            column_names: Arc::new(column_names),
                            columns,
                            parameters: Vec::default(),
                        });
                    }

                    BackendMessageFormat::DataRow => {
                        logger.increment_rows_returned();

                        // one of the set of rows returned by a SELECT, FETCH, etc query
                        let data: DataRow = message.decode()?;
                        let row = PgRow {
                            data,
                            format,
                            metadata: Arc::clone(&metadata),
                        };

                        r#yield!(Either::Right(row));
                    }

                    BackendMessageFormat::ReadyForQuery => {
                        // Processing of the query string is complete, the transaction status is
                        // updated in the bg worker.
                        break;
                    }

                    _ => {
                        return Err(err_protocol!(
                            "execute: unexpected message: {:?}",
                            message.format
                        ));
                    }
                }
            }

            Ok(())
        })
    }
}

impl<'c> Executor<'c> for &'c mut PgConnection {
    type Database = Postgres;

    fn fetch_many<'e, 'q, E>(
        self,
        mut query: E,
    ) -> BoxStream<'e, Result<Either<PgQueryResult, PgRow>, Error>>
    where
        'c: 'e,
        E: Execute<'q, Self::Database>,
        'q: 'e,
        E: 'q,
    {
        // False positive: https://github.com/rust-lang/rust-clippy/issues/12560
        #[allow(clippy::map_clone)]
        let metadata = query.statement().map(|s| Arc::clone(&s.metadata));
        let arguments = query.take_arguments().map_err(Error::Encode);
        let persistent = query.persistent();
        let sql = query.sql();

        Box::pin(try_stream! {
            let arguments = arguments?;
            let mut s = pin!(self.run(sql, arguments, persistent, metadata).await?);

            while let Some(v) = s.try_next().await? {
                r#yield!(v);
            }

            Ok(())
        })
    }

    fn fetch_optional<'e, 'q, E>(self, mut query: E) -> BoxFuture<'e, Result<Option<PgRow>, Error>>
    where
        'c: 'e,
        E: Execute<'q, Self::Database>,
        'q: 'e,
        E: 'q,
    {
        // False positive: https://github.com/rust-lang/rust-clippy/issues/12560
        #[allow(clippy::map_clone)]
        let metadata = query.statement().map(|s| Arc::clone(&s.metadata));
        let arguments = query.take_arguments().map_err(Error::Encode);
        let persistent = query.persistent();

        Box::pin(async move {
            let sql = query.sql();
            let arguments = arguments?;
            let mut s = pin!(self.run(sql, arguments, persistent, metadata).await?);

            // With deferred constraints we need to check all responses as we
            // could get a OK response (with uncommitted data), only to get an
            // error response after (when the deferred constraint is actually
            // checked).
            let mut ret = None;
            while let Some(result) = s.try_next().await? {
                match result {
                    Either::Right(r) if ret.is_none() => ret = Some(r),
                    _ => {}
                }
            }
            Ok(ret)
        })
    }

    fn prepare_with<'e>(
        self,
        sql: SqlStr,
        parameters: &'e [PgTypeInfo],
    ) -> BoxFuture<'e, Result<PgStatement, Error>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            let (_, metadata) = self
                .get_or_prepare(sql.as_str(), parameters, true, None, true)
                .await?;

            Ok(PgStatement { sql, metadata })
        })
    }

    #[cfg(feature = "offline")]
    fn describe<'e>(
        self,
        sql: SqlStr,
    ) -> BoxFuture<'e, Result<crate::describe::Describe<Self::Database>, Error>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            let (stmt_id, metadata) = self
                .get_or_prepare(sql.as_str(), &[], true, None, true)
                .await?;

            let nullable = self.get_nullable_for_columns(stmt_id, &metadata).await?;

            Ok(crate::describe::Describe {
                columns: metadata.columns.clone(),
                nullable,
                parameters: Some(Either::Left(metadata.parameters.clone())),
            })
        })
    }
}
