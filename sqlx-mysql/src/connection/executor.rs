use super::MySqlStream;
use crate::connection::stream::Waiting;
use crate::error::Error;
use crate::executor::{Execute, Executor};
use crate::ext::ustr::UStr;
use crate::io::MySqlBufExt;
use crate::logger::QueryLogger;
use crate::protocol::response::Status;
use crate::protocol::statement::{
    BinaryRow, Execute as StatementExecute, Prepare, PrepareOk, StmtClose,
};
use crate::protocol::text::{ColumnDefinition, Query, TextRow};
use crate::statement::{MySqlStatement, MySqlStatementMetadata};
use crate::HashMap;
use crate::{
    MySql, MySqlArguments, MySqlColumn, MySqlConnection, MySqlQueryResult, MySqlRow, MySqlTypeInfo,
    MySqlValueFormat,
};
use either::Either;
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_core::Stream;
use futures_util::TryStreamExt;
use sqlx_core::column::{ColumnOrigin, TableColumn};
use sqlx_core::sql_str::SqlStr;
use std::{pin::pin, sync::Arc};

impl MySqlConnection {
    async fn prepare_statement(
        &mut self,
        sql: &str,
    ) -> Result<(u32, MySqlStatementMetadata), Error> {
        // https://dev.mysql.com/doc/internals/en/com-stmt-prepare.html
        // https://dev.mysql.com/doc/internals/en/com-stmt-prepare-response.html#packet-COM_STMT_PREPARE_OK

        self.inner
            .stream
            .send_packet(Prepare { query: sql })
            .await?;

        let ok: PrepareOk = self.inner.stream.recv().await?;

        // the parameter definitions are very unreliable so we skip over them
        // as we have little use

        if ok.params > 0 {
            for _ in 0..ok.params {
                let _def: ColumnDefinition = self.inner.stream.recv().await?;
            }

            self.inner.stream.maybe_recv_eof().await?;
        }

        // the column definitions are berefit the type information from the
        // to-be-bound parameters; we will receive the output column definitions
        // once more on execute so we wait for that

        let mut columns = Vec::new();

        let column_names = if ok.columns > 0 {
            recv_result_metadata(&mut self.inner.stream, ok.columns as usize, &mut columns).await?
        } else {
            Default::default()
        };

        let id = ok.statement_id;
        let metadata = MySqlStatementMetadata {
            parameters: ok.params as usize,
            columns: Arc::new(columns),
            column_names: Arc::new(column_names),
        };

        Ok((id, metadata))
    }

    async fn get_or_prepare_statement(
        &mut self,
        sql: &str,
    ) -> Result<(u32, MySqlStatementMetadata), Error> {
        if let Some(statement) = self.inner.cache_statement.get_mut(sql) {
            // <MySqlStatementMetadata> is internally reference-counted
            return Ok((*statement).clone());
        }

        let (id, metadata) = self.prepare_statement(sql).await?;

        // in case of the cache being full, close the least recently used statement
        if let Some((id, _)) = self
            .inner
            .cache_statement
            .insert(sql, (id, metadata.clone()))
        {
            self.inner
                .stream
                .send_packet(StmtClose { statement: id })
                .await?;
        }

        Ok((id, metadata))
    }

    #[allow(clippy::needless_lifetimes)]
    pub(crate) async fn run<'e, 'c: 'e, 'q: 'e>(
        &'c mut self,
        sql: SqlStr,
        arguments: Option<MySqlArguments>,
        persistent: bool,
    ) -> Result<impl Stream<Item = Result<Either<MySqlQueryResult, MySqlRow>, Error>> + 'e, Error>
    {
        let mut logger = QueryLogger::new(sql, self.inner.log_settings.clone());

        self.inner.stream.wait_until_ready().await?;
        self.inner.stream.waiting.push_back(Waiting::Result);

        Ok(try_stream! {
        let sql = logger.sql().as_str();

            // make a slot for the shared column data
            // as long as a reference to a row is not held past one iteration, this enables us
            // to re-use this memory freely between result sets
            let mut columns = Arc::new(Vec::new());

            let format = if let Some(arguments) = arguments {
                if persistent && self.inner.cache_statement.is_enabled() {
                    let (id, metadata) = self
                        .get_or_prepare_statement(sql)
                        .await?;

                    if arguments.types.len() != metadata.parameters {
                        return Err(
                            err_protocol!(
                                "prepared statement expected {} parameters but {} parameters were provided",
                                metadata.parameters,
                                arguments.types.len()
                            )
                        );
                    }

                    // https://dev.mysql.com/doc/internals/en/com-stmt-execute.html
                    self.inner.stream
                        .send_packet(StatementExecute {
                            statement: id,
                            arguments: &arguments,
                        })
                        .await?;

                    MySqlValueFormat::Binary
                } else {
                    let (id, metadata) = self
                        .prepare_statement(sql)
                        .await?;

                    if arguments.types.len() != metadata.parameters {
                        return Err(
                            err_protocol!(
                                "prepared statement expected {} parameters but {} parameters were provided",
                                metadata.parameters,
                                arguments.types.len()
                            )
                        );
                    }

                    // https://dev.mysql.com/doc/internals/en/com-stmt-execute.html
                    self.inner.stream
                        .send_packet(StatementExecute {
                            statement: id,
                            arguments: &arguments,
                        })
                        .await?;

                    self.inner.stream.send_packet(StmtClose { statement: id }).await?;

                    MySqlValueFormat::Binary
                }
            } else {
                // https://dev.mysql.com/doc/internals/en/com-query.html
                self.inner.stream.send_packet(Query(sql)).await?;

                MySqlValueFormat::Text
            };

            loop {
                // query response is a meta-packet which may be one of:
                //  Ok, Err, ResultSet, or (unhandled) LocalInfileRequest
                let mut packet = self.inner.stream.recv_packet().await?;

                if packet[0] == 0x00 || packet[0] == 0xff {
                    // first packet in a query response is OK or ERR
                    // this indicates either a successful query with no rows at all or a failed query
                    let ok = packet.ok()?;

                    self.inner.status_flags = ok.status;

                    let rows_affected = ok.affected_rows;
                    logger.increase_rows_affected(rows_affected);
                    let done = MySqlQueryResult {
                        rows_affected,
                        last_insert_id: ok.last_insert_id,
                    };

                    r#yield!(Either::Left(done));

                    if ok.status.contains(Status::SERVER_MORE_RESULTS_EXISTS) {
                        // more result sets exist, continue to the next one
                        continue;
                    }

                    self.inner.stream.waiting.pop_front();
                    return Ok(());
                }

                // otherwise, this first packet is the start of the result-set metadata,
                *self.inner.stream.waiting.front_mut().unwrap() = Waiting::Row;

                let num_columns = packet.get_uint_lenenc()?; // column count
                let num_columns = usize::try_from(num_columns)
                    .map_err(|_| err_protocol!("column count overflows usize: {num_columns}"))?;

                // Always reload column names, even for prepared statements (the schema
                // may change between PREPARE and EXECUTE).
                let column_names = Arc::new(recv_result_metadata(&mut self.inner.stream, num_columns, Arc::make_mut(&mut columns)).await?);

                // finally, there will be none or many result-rows
                loop {
                    let packet = self.inner.stream.recv_packet().await?;

                    if packet[0] == 0xfe {
                        let (rows_affected, last_insert_id, status) = if packet.len() < 9 {
                            // EOF packet
                            let eof = packet.eof(self.inner.stream.capabilities)?;
                            (0, 0, eof.status)
                        } else {
                            // OK packet
                            let ok = packet.ok()?;
                            (ok.affected_rows, ok.last_insert_id, ok.status)
                        };

                        self.inner.status_flags = status;
                        r#yield!(Either::Left(MySqlQueryResult {
                            rows_affected,
                            last_insert_id,
                        }));

                        if status.contains(Status::SERVER_MORE_RESULTS_EXISTS) {
                            *self.inner.stream.waiting.front_mut().unwrap() = Waiting::Result;
                            break;
                        }
                        self.inner.stream.waiting.pop_front();
                        return Ok(());
                    }

                    let row = match format {
                        MySqlValueFormat::Binary => packet.decode_with::<BinaryRow, _>(&columns)?.0,
                        MySqlValueFormat::Text => packet.decode_with::<TextRow, _>(&columns)?.0,
                    };

                    let v = Either::Right(MySqlRow {
                        row,
                        format,
                        columns: Arc::clone(&columns),
                        column_names: Arc::clone(&column_names),
                    });

                    logger.increment_rows_returned();

                    r#yield!(v);
                }
            }
        })
    }
}

impl<'c> Executor<'c> for &'c mut MySqlConnection {
    type Database = MySql;

    fn fetch_many<'e, 'q, E>(
        self,
        mut query: E,
    ) -> BoxStream<'e, Result<Either<MySqlQueryResult, MySqlRow>, Error>>
    where
        'c: 'e,
        E: Execute<'q, Self::Database>,
        'q: 'e,
        E: 'q,
    {
        let arguments = query.take_arguments().map_err(Error::Encode);
        let persistent = query.persistent();

        Box::pin(try_stream! {
        let sql = query.sql();
            let arguments = arguments?;
            let mut s = pin!(self.run(sql, arguments, persistent).await?);

            while let Some(v) = s.try_next().await? {
                r#yield!(v);
            }

            Ok(())
        })
    }

    fn fetch_optional<'e, 'q, E>(self, query: E) -> BoxFuture<'e, Result<Option<MySqlRow>, Error>>
    where
        'c: 'e,
        E: Execute<'q, Self::Database>,
        'q: 'e,
        E: 'q,
    {
        let mut s = self.fetch_many(query);

        Box::pin(async move {
            while let Some(v) = s.try_next().await? {
                if let Either::Right(r) = v {
                    return Ok(Some(r));
                }
            }

            Ok(None)
        })
    }

    fn prepare_with<'e>(
        self,
        sql: SqlStr,
        _parameters: &'e [MySqlTypeInfo],
    ) -> BoxFuture<'e, Result<MySqlStatement, Error>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            self.inner.stream.wait_until_ready().await?;

            let metadata = if self.inner.cache_statement.is_enabled() {
                self.get_or_prepare_statement(sql.as_str()).await?.1
            } else {
                let (id, metadata) = self.prepare_statement(sql.as_str()).await?;

                self.inner
                    .stream
                    .send_packet(StmtClose { statement: id })
                    .await?;

                metadata
            };

            Ok(MySqlStatement {
                sql,
                // metadata has internal Arcs for expensive data structures
                metadata: metadata.clone(),
            })
        })
    }

    #[doc(hidden)]
    #[cfg(feature = "offline")]
    fn describe<'e>(
        self,
        sql: SqlStr,
    ) -> BoxFuture<'e, Result<crate::describe::Describe<MySql>, Error>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            self.inner.stream.wait_until_ready().await?;

            let (id, metadata) = self.prepare_statement(sql.as_str()).await?;

            self.inner
                .stream
                .send_packet(StmtClose { statement: id })
                .await?;

            let columns = (*metadata.columns).clone();

            let nullable = columns
                .iter()
                .map(|col| {
                    col.flags
                        .map(|flags| !flags.contains(crate::protocol::text::ColumnFlags::NOT_NULL))
                })
                .collect();

            Ok(crate::describe::Describe {
                parameters: Some(Either::Right(metadata.parameters)),
                columns,
                nullable,
            })
        })
    }
}

fn recv_next_result_column(def: &ColumnDefinition, ordinal: usize) -> Result<MySqlColumn, Error> {
    // if the alias is empty, use the alias
    // only then use the name
    let column_name = def.name()?;

    let name = match (def.name()?, def.alias()?) {
        (_, alias) if !alias.is_empty() => UStr::new(alias),
        (name, _) => UStr::new(name),
    };

    let table = def.table()?;

    let origin = if table.is_empty() {
        ColumnOrigin::Expression
    } else {
        let schema = def.schema()?;

        ColumnOrigin::Table(TableColumn {
            table: if !schema.is_empty() {
                format!("{schema}.{table}").into()
            } else {
                table.into()
            },
            name: column_name.into(),
        })
    };

    let type_info = MySqlTypeInfo::from_column(def);

    Ok(MySqlColumn {
        name,
        type_info,
        ordinal,
        flags: Some(def.flags),
        origin,
    })
}

async fn recv_result_metadata(
    stream: &mut MySqlStream,
    num_columns: usize,
    columns: &mut Vec<MySqlColumn>,
) -> Result<HashMap<UStr, usize>, Error> {
    // the result-set metadata is primarily a listing of each output
    // column in the result-set

    let mut column_names = HashMap::with_capacity(num_columns);

    columns.clear();
    columns.reserve(num_columns);

    for ordinal in 0..num_columns {
        let def: ColumnDefinition = stream.recv().await?;

        let column = recv_next_result_column(&def, ordinal)?;

        column_names.insert(column.name.clone(), ordinal);
        columns.push(column);
    }

    stream.maybe_recv_eof().await?;

    Ok(column_names)
}
