use crate::database::MssqlArgumentValue;
use crate::error::{tiberius_err, Error};
use crate::executor::{Execute, Executor};
use crate::ext::ustr::UStr;
use crate::logger::QueryLogger;
use crate::statement::{MssqlStatement, MssqlStatementMetadata};
use crate::type_info::{type_name_for_tiberius, MssqlTypeInfo};
use crate::value::{column_data_to_mssql_data, MssqlData};
use crate::HashMap;
use crate::{Mssql, MssqlArguments, MssqlColumn, MssqlConnection, MssqlQueryResult, MssqlRow};
use either::Either;
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_util::TryStreamExt;
use sqlx_core::column::{ColumnOrigin, TableColumn};
use sqlx_core::sql_str::{AssertSqlSafe, SqlSafeStr as _, SqlStr};
use std::sync::Arc;

/// Newtype wrapper to bridge `tiberius::ColumnData` into `tiberius::IntoSql`.
///
/// tiberius implements `ToSql` but not `IntoSql` for some types (e.g. `time`
/// crate types, and `BigDecimal` due to version mismatch). `Query::bind()`
/// requires `IntoSql`, so this wrapper lets us construct `ColumnData` manually
/// and pass it to `bind()`.
#[cfg(any(feature = "chrono", feature = "time", feature = "bigdecimal"))]
struct ColumnDataWrapper<'a>(tiberius::ColumnData<'a>);

#[cfg(any(feature = "chrono", feature = "time", feature = "bigdecimal"))]
impl<'a> tiberius::IntoSql<'a> for ColumnDataWrapper<'a> {
    fn into_sql(self) -> tiberius::ColumnData<'a> {
        self.0
    }
}

/// Maximum days-since-epoch (0001-01-01) that fits in the 3-byte TDS date
/// encoding. `tiberius::time::Date::new()` panics if `days > 0x00FFFFFF`.
#[cfg(any(feature = "chrono", feature = "time"))]
const MAX_DAYS: u32 = 0x00FF_FFFF;

/// Convert a signed days-since-epoch count to `u32`, returning
/// `Error::Encode` if negative or exceeding the TDS 3-byte limit.
#[cfg(any(feature = "chrono", feature = "time"))]
fn days_since_epoch_to_u32(days: i64) -> Result<u32, Error> {
    u32::try_from(days)
        .ok()
        .filter(|&d| d <= MAX_DAYS)
        .ok_or_else(|| {
            Error::Encode(
                format!(
                    "date out of range for SQL Server: {days} days since epoch \
                     (must be 0..={MAX_DAYS})"
                )
                .into(),
            )
        })
}

/// Convert a signed offset-in-minutes to `i16`, returning
/// `Error::Encode` if outside the SQL Server range (-840..=840).
#[cfg(any(feature = "chrono", feature = "time"))]
fn offset_minutes_to_i16(offset_minutes: i32) -> Result<i16, Error> {
    const MIN_OFFSET: i32 = -840;
    const MAX_OFFSET: i32 = 840;
    if (MIN_OFFSET..=MAX_OFFSET).contains(&offset_minutes) {
        // SAFETY: range check above guarantees -840..=840, which fits in i16.
        #[allow(clippy::cast_possible_truncation)]
        Ok(offset_minutes as i16)
    } else {
        Err(Error::Encode(
            format!(
                "timezone offset out of range for SQL Server: {offset_minutes} minutes \
                 (must be {MIN_OFFSET}..={MAX_OFFSET})"
            )
            .into(),
        ))
    }
}

/// Convert a `BigDecimal` into the `(i128, u8)` pair that
/// `tiberius::numeric::Numeric::new_with_scale` expects.
///
/// Handles two edge cases:
/// - **Negative exponents** (e.g. `BigDecimal(9, -3)` = 9000): rescales to
///   exponent 0 so SQL Server receives the correct magnitude.
/// - **Scale > 37**: SQL Server NUMERIC max scale is 37, and tiberius
///   asserts `scale < 38`. Returns `Error::Encode` instead of panicking.
#[cfg(feature = "bigdecimal")]
fn bigdecimal_to_numeric(v: &bigdecimal::BigDecimal) -> Result<(i128, u8), Error> {
    use bigdecimal::ToPrimitive;

    let (bigint, exponent) = v.as_bigint_and_exponent();
    let (bigint, exponent) = if exponent < 0 {
        v.with_scale(0).into_bigint_and_exponent()
    } else {
        (bigint, exponent)
    };

    if exponent > 37 {
        return Err(Error::Encode(
            format!("BigDecimal scale {exponent} exceeds SQL Server maximum of 37").into(),
        ));
    }
    // SAFETY: guarded by `exponent > 37` check above; 0..=37 fits in u8.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scale = exponent as u8;

    let value: i128 = bigint.to_i128().ok_or_else(|| {
        Error::Encode(format!("BigDecimal value too large for SQL NUMERIC: {v}").into())
    })?;

    Ok((value, scale))
}

impl MssqlConnection {
    /// Execute a query, eagerly collecting all results.
    ///
    /// We collect eagerly because `tiberius::QueryStream` borrows `&mut Client`,
    /// which prevents us from holding it across yield points alongside `&mut self`.
    pub(crate) async fn run(
        &mut self,
        sql: &str,
        arguments: Option<MssqlArguments>,
    ) -> Result<Vec<Either<MssqlQueryResult, MssqlRow>>, Error> {
        // Resolve any pending rollback first
        crate::transaction::resolve_pending_rollback(self).await?;

        let mut logger = QueryLogger::new(
            AssertSqlSafe(sql).into_sql_str(),
            self.inner.log_settings.clone(),
        );

        let mut results = Vec::new();

        if let Some(args) = arguments {
            // Parameterized query using tiberius::Query
            let mut query = tiberius::Query::new(sql);
            bind_arguments(&mut query, &args)?;

            let stream = query
                .query(&mut self.inner.client)
                .await
                .map_err(tiberius_err)?;
            collect_results(stream, &mut results, &mut logger).await?;
        } else {
            // Simple query (no parameters)
            let stream = self
                .inner
                .client
                .simple_query(sql)
                .await
                .map_err(tiberius_err)?;
            collect_results(stream, &mut results, &mut logger).await?;
        }

        Ok(results)
    }

    /// Execute a parameterized statement and return the affected-row count(s)
    /// reported by SQL Server.
    ///
    /// Unlike [`run`], which uses tiberius `query()` (a `QueryStream` that
    /// silently drops the `DONE`/`DONEINPROC` tokens carrying the affected-row
    /// count), this drives tiberius `execute()`, whose `ExecuteResult` reads
    /// those tokens. This is why `rows_affected()` is accurate for
    /// `INSERT`/`UPDATE`/`DELETE` here but not via the fetch path.
    ///
    /// One [`MssqlQueryResult`] is returned per statement in the batch, matching
    /// `execute_many`'s one-result-per-statement contract.
    ///
    /// This path is parameterized only: it goes through `sp_executesql`, so it
    /// must not be used for DDL such as `CREATE PROCEDURE`/`VIEW`/`TRIGGER` that
    /// must be the first statement in a plain batch — those stay on the
    /// no-parameter [`run`] (`simple_query`) path.
    pub(crate) async fn run_execute(
        &mut self,
        sql: &str,
        arguments: MssqlArguments,
    ) -> Result<Vec<MssqlQueryResult>, Error> {
        // Resolve any pending rollback first
        crate::transaction::resolve_pending_rollback(self).await?;

        let mut logger = QueryLogger::new(
            AssertSqlSafe(sql).into_sql_str(),
            self.inner.log_settings.clone(),
        );

        let mut query = tiberius::Query::new(sql);
        bind_arguments(&mut query, &arguments)?;

        let result = query
            .execute(&mut self.inner.client)
            .await
            .map_err(tiberius_err)?;

        Ok(result
            .rows_affected()
            .iter()
            .map(|&rows_affected| {
                logger.increase_rows_affected(rows_affected);
                MssqlQueryResult { rows_affected }
            })
            .collect())
    }
}

/// Bind a query's arguments onto a `tiberius::Query`.
///
/// Shared by [`MssqlConnection::run`] (fetch path) and
/// [`MssqlConnection::run_execute`] (execute path). The `'a` lifetime ties
/// borrowed argument data (e.g. `String`/`Vec<u8>`) to `args`.
fn bind_arguments<'a>(
    query: &mut tiberius::Query<'a>,
    args: &'a MssqlArguments,
) -> Result<(), Error> {
    for arg in &args.values {
        match arg {
            MssqlArgumentValue::Null => {
                query.bind(Option::<&str>::None);
            }
            MssqlArgumentValue::Bool(v) => {
                query.bind(*v);
            }
            MssqlArgumentValue::U8(v) => {
                query.bind(*v);
            }
            MssqlArgumentValue::I16(v) => {
                query.bind(*v);
            }
            MssqlArgumentValue::I32(v) => {
                query.bind(*v);
            }
            MssqlArgumentValue::I64(v) => {
                query.bind(*v);
            }
            MssqlArgumentValue::F32(v) => {
                query.bind(*v);
            }
            MssqlArgumentValue::F64(v) => {
                query.bind(*v);
            }
            MssqlArgumentValue::String(v) => {
                query.bind(v.as_str());
            }
            MssqlArgumentValue::Binary(v) => {
                query.bind(v.as_slice());
            }
            #[cfg(feature = "chrono")]
            MssqlArgumentValue::NaiveDateTime(v) => {
                query.bind(*v);
            }
            #[cfg(feature = "chrono")]
            MssqlArgumentValue::NaiveDate(v) => {
                query.bind(*v);
            }
            #[cfg(feature = "chrono")]
            MssqlArgumentValue::NaiveTime(v) => {
                query.bind(*v);
            }
            #[cfg(feature = "chrono")]
            MssqlArgumentValue::DateTimeFixedOffset(v) => {
                use chrono::Timelike as _;
                let epoch = chrono::NaiveDate::from_ymd_opt(1, 1, 1)
                    .expect("epoch 0001-01-01 is always valid");
                let naive = v.naive_local();
                let days = days_since_epoch_to_u32((naive.date() - epoch).num_days())?;
                let time = naive.time();
                let total_ns = u64::from(time.num_seconds_from_midnight()) * 1_000_000_000
                    + (u64::from(time.nanosecond()) % 1_000_000_000);
                let increments = total_ns / 100;
                let offset_minutes = v.offset().local_minus_utc() / 60;
                let dt2 = tiberius::time::DateTime2::new(
                    tiberius::time::Date::new(days),
                    tiberius::time::Time::new(increments, 7),
                );
                let cd = tiberius::ColumnData::DateTimeOffset(Some(
                    tiberius::time::DateTimeOffset::new(dt2, offset_minutes_to_i16(offset_minutes)?),
                ));
                query.bind(ColumnDataWrapper(cd));
            }
            #[cfg(feature = "uuid")]
            MssqlArgumentValue::Uuid(v) => {
                query.bind(v);
            }
            #[cfg(feature = "rust_decimal")]
            MssqlArgumentValue::Decimal(v) => {
                let unpacked = v.unpack();
                // SAFETY: rust_decimal mantissa is ≤96 bits (hi:mid:lo are u32s), fits in i128.
                #[allow(clippy::cast_possible_wrap)]
                let mut value = (((unpacked.hi as u128) << 64)
                    + ((unpacked.mid as u128) << 32)
                    + unpacked.lo as u128) as i128;
                if v.is_sign_negative() {
                    value = -value;
                }
                let scale = v.scale();
                if scale > 37 {
                    return Err(Error::Encode(
                        format!("rust_decimal scale {scale} exceeds SQL Server maximum of 37")
                            .into(),
                    ));
                }
                // SAFETY: guarded by `scale > 37` check above; 0..=37 fits in u8.
                #[allow(clippy::cast_possible_truncation)]
                let scale_u8 = scale as u8;
                query.bind(tiberius::numeric::Numeric::new_with_scale(value, scale_u8));
            }
            #[cfg(feature = "time")]
            MssqlArgumentValue::TimeDate(v) => {
                let epoch = time::Date::from_ordinal_date(1, 1)
                    .expect("epoch 0001-01-01 is always valid");
                let days = days_since_epoch_to_u32((*v - epoch).whole_days())?;
                let cd = tiberius::ColumnData::Date(Some(tiberius::time::Date::new(days)));
                query.bind(ColumnDataWrapper(cd));
            }
            #[cfg(feature = "time")]
            MssqlArgumentValue::TimeTime(v) => {
                let (h, m, s, ns) = v.as_hms_nano();
                let total_ns = u64::from(h) * 3_600_000_000_000
                    + u64::from(m) * 60_000_000_000
                    + u64::from(s) * 1_000_000_000
                    + u64::from(ns);
                // Scale 7 = 100ns increments
                let increments = total_ns / 100;
                let cd = tiberius::ColumnData::Time(Some(tiberius::time::Time::new(increments, 7)));
                query.bind(ColumnDataWrapper(cd));
            }
            #[cfg(feature = "time")]
            MssqlArgumentValue::TimePrimitiveDateTime(v) => {
                let date = v.date();
                let time = v.time();
                let epoch = time::Date::from_ordinal_date(1, 1)
                    .expect("epoch 0001-01-01 is always valid");
                let days = days_since_epoch_to_u32((date - epoch).whole_days())?;
                let (h, m, s, ns) = time.as_hms_nano();
                let total_ns = u64::from(h) * 3_600_000_000_000
                    + u64::from(m) * 60_000_000_000
                    + u64::from(s) * 1_000_000_000
                    + u64::from(ns);
                let increments = total_ns / 100;
                let cd = tiberius::ColumnData::DateTime2(Some(tiberius::time::DateTime2::new(
                    tiberius::time::Date::new(days),
                    tiberius::time::Time::new(increments, 7),
                )));
                query.bind(ColumnDataWrapper(cd));
            }
            #[cfg(feature = "time")]
            MssqlArgumentValue::TimeOffsetDateTime(v) => {
                let epoch = time::Date::from_ordinal_date(1, 1)
                    .expect("epoch 0001-01-01 is always valid");
                let offset_minutes = v.offset().whole_seconds() / 60;
                let date = v.date();
                let time = v.time();
                let days = days_since_epoch_to_u32((date - epoch).whole_days())?;
                let (h, m, s, ns) = time.as_hms_nano();
                let total_ns = u64::from(h) * 3_600_000_000_000
                    + u64::from(m) * 60_000_000_000
                    + u64::from(s) * 1_000_000_000
                    + u64::from(ns);
                let increments = total_ns / 100;
                let dt2 = tiberius::time::DateTime2::new(
                    tiberius::time::Date::new(days),
                    tiberius::time::Time::new(increments, 7),
                );
                let cd = tiberius::ColumnData::DateTimeOffset(Some(
                    tiberius::time::DateTimeOffset::new(dt2, offset_minutes_to_i16(offset_minutes)?),
                ));
                query.bind(ColumnDataWrapper(cd));
            }
            #[cfg(feature = "bigdecimal")]
            MssqlArgumentValue::BigDecimal(v) => {
                let (value, scale) = bigdecimal_to_numeric(v)?;
                let cd = tiberius::ColumnData::Numeric(Some(
                    tiberius::numeric::Numeric::new_with_scale(value, scale),
                ));
                query.bind(ColumnDataWrapper(cd));
            }
        }
    }

    Ok(())
}

/// Collect all results from a tiberius QueryStream into a Vec.
async fn collect_results(
    mut stream: tiberius::QueryStream<'_>,
    results: &mut Vec<Either<MssqlQueryResult, MssqlRow>>,
    logger: &mut QueryLogger,
) -> Result<(), Error> {
    // Process all result sets
    let mut columns: Option<Arc<Vec<MssqlColumn>>> = None;
    let mut column_names: Option<Arc<HashMap<UStr, usize>>> = None;
    let mut rows_affected: u64 = 0;

    while let Some(item) = stream.try_next().await.map_err(tiberius_err)? {
        match item {
            tiberius::QueryItem::Metadata(meta) => {
                // Build column info from metadata
                let cols: Vec<MssqlColumn> = meta
                    .columns()
                    .iter()
                    .enumerate()
                    .map(|(ordinal, col)| {
                        let name = UStr::new(col.name());
                        let type_info =
                            MssqlTypeInfo::new(type_name_for_tiberius(&col.column_type()));
                        MssqlColumn {
                            ordinal,
                            name,
                            type_info,
                            origin: ColumnOrigin::Unknown,
                        }
                    })
                    .collect();

                let names: HashMap<UStr, usize> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, col)| (col.name.clone(), i))
                    .collect();

                columns = Some(Arc::new(cols));
                column_names = Some(Arc::new(names));
            }
            tiberius::QueryItem::Row(row) => {
                let cols = columns
                    .as_ref()
                    .ok_or_else(|| Error::Protocol("row received before metadata".into()))?;
                let names = column_names
                    .as_ref()
                    .ok_or_else(|| Error::Protocol("row received before metadata".into()))?;

                // Convert tiberius row to MssqlRow by iterating over cells
                let values: Vec<MssqlData> = row
                    .into_iter()
                    .map(column_data_to_mssql_data)
                    .collect::<Result<Vec<_>, _>>()?;

                rows_affected += 1;
                logger.increment_rows_returned();
                results.push(Either::Right(MssqlRow {
                    values,
                    columns: Arc::clone(cols),
                    column_names: Arc::clone(names),
                }));
            }
        }
    }

    // Report query result with total rows
    logger.increase_rows_affected(rows_affected);
    results.push(Either::Left(MssqlQueryResult { rows_affected }));

    Ok(())
}

/// Build column metadata from `sp_describe_first_result_set` result rows.
///
/// Returns `(columns, column_names, nullable)` where `nullable` contains one
/// `Option<bool>` per column (extracted from the `is_nullable` field).
fn build_columns_from_describe_rows(
    rows: &[tiberius::Row],
) -> (Vec<MssqlColumn>, HashMap<UStr, usize>, Vec<Option<bool>>) {
    let mut columns = Vec::with_capacity(rows.len());
    let mut column_names = HashMap::with_capacity(rows.len());
    let mut nullable = Vec::with_capacity(rows.len());

    for (ordinal, row) in rows.iter().enumerate() {
        let name: &str = row.get("name").unwrap_or("");
        let type_name: &str = row.get("system_type_name").unwrap_or("UNKNOWN");
        let type_info = MssqlTypeInfo::new(type_name.to_uppercase());
        let is_nullable: Option<bool> = row.get("is_nullable");

        let source_table: Option<&str> = row.get("source_table");
        let source_schema: Option<&str> = row.get("source_schema");
        let source_column: Option<&str> = row.get("source_column");

        let origin = match (source_table, source_column) {
            (Some(table), Some(col)) if !table.is_empty() && !col.is_empty() => {
                let table_str = match source_schema {
                    Some(s) if !s.is_empty() => format!("{s}.{table}"),
                    _ => table.to_string(),
                };
                ColumnOrigin::Table(TableColumn {
                    table: table_str.into(),
                    name: col.into(),
                })
            }
            _ => ColumnOrigin::Expression,
        };

        let ustr_name = UStr::new(name);
        column_names.insert(ustr_name.clone(), ordinal);
        columns.push(MssqlColumn {
            ordinal,
            name: ustr_name,
            type_info,
            origin,
        });
        nullable.push(is_nullable);
    }

    (columns, column_names, nullable)
}

impl<'c> Executor<'c> for &'c mut MssqlConnection {
    type Database = Mssql;

    fn fetch_many<'e, 'q, E>(
        self,
        mut query: E,
    ) -> BoxStream<'e, Result<Either<MssqlQueryResult, MssqlRow>, Error>>
    where
        'c: 'e,
        E: Execute<'q, Self::Database>,
        'q: 'e,
        E: 'q,
    {
        let arguments = query.take_arguments().map_err(Error::Encode);
        // MSSQL always sends parameterized queries via tiberius — there is no
        // server-side prepared statement caching like PostgreSQL's, so this
        // flag is intentionally unused.
        let _persistent = query.persistent();
        let sql = query.sql();

        Box::pin(
            futures_util::stream::once(async move {
                let arguments = arguments?;
                let results = self.run(sql.as_str(), arguments).await?;
                Ok::<_, Error>(results)
            })
            .map_ok(|results| futures_util::stream::iter(results.into_iter().map(Ok)))
            .try_flatten(),
        )
    }

    fn execute_many<'e, 'q, E>(
        self,
        mut query: E,
    ) -> BoxStream<'e, Result<MssqlQueryResult, Error>>
    where
        'c: 'e,
        E: Execute<'q, Self::Database>,
        'q: 'e,
        E: 'q,
    {
        let arguments = query.take_arguments().map_err(Error::Encode);
        let sql = query.sql();

        Box::pin(
            futures_util::stream::once(async move {
                let results = match arguments? {
                    // Parameterized statements go through tiberius `execute()`
                    // (sp_executesql), whose `ExecuteResult` carries the real
                    // affected-row count reported by SQL Server.
                    Some(args) => self.run_execute(sql.as_str(), args).await?,
                    // No-parameter statements stay on the plain-batch
                    // `simple_query` path, which is required for DDL such as
                    // `CREATE PROCEDURE`/`VIEW`/`TRIGGER` in migrations. That
                    // path cannot read the affected-row count, so `rows_affected`
                    // is best-effort there; bind your values and virtually all
                    // real DML hits the accurate path above.
                    None => self
                        .run(sql.as_str(), None)
                        .await?
                        .into_iter()
                        .filter_map(Either::left)
                        .collect(),
                };
                Ok::<_, Error>(futures_util::stream::iter(results.into_iter().map(Ok)))
            })
            .try_flatten(),
        )
    }

    fn fetch_optional<'e, 'q, E>(self, query: E) -> BoxFuture<'e, Result<Option<MssqlRow>, Error>>
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
        _parameters: &'e [MssqlTypeInfo],
    ) -> BoxFuture<'e, Result<MssqlStatement, Error>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            let mut describe_query =
                tiberius::Query::new("EXEC sp_describe_first_result_set @tsql = @p1");
            describe_query.bind(sql.as_str());

            let stream = describe_query
                .query(&mut self.inner.client)
                .await
                .map_err(tiberius_err)?;

            let rows: Vec<tiberius::Row> =
                stream.into_first_result().await.map_err(tiberius_err)?;
            let (columns, column_names, _nullable) = build_columns_from_describe_rows(&rows);

            Ok(MssqlStatement {
                sql,
                metadata: MssqlStatementMetadata {
                    columns: Arc::new(columns),
                    column_names: Arc::new(column_names),
                    parameters: 0,
                },
            })
        })
    }

    #[doc(hidden)]
    #[cfg(feature = "offline")]
    fn describe<'e>(
        self,
        sql: SqlStr,
    ) -> BoxFuture<'e, Result<crate::describe::Describe<Mssql>, Error>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            let mut describe_query =
                tiberius::Query::new("EXEC sp_describe_first_result_set @tsql = @p1");
            describe_query.bind(sql.as_str());

            let stream = describe_query
                .query(&mut self.inner.client)
                .await
                .map_err(tiberius_err)?;

            let rows: Vec<tiberius::Row> =
                stream.into_first_result().await.map_err(tiberius_err)?;

            let (columns, _column_names, nullable) = build_columns_from_describe_rows(&rows);

            // Count parameters using sp_describe_undeclared_parameters
            let mut param_query =
                tiberius::Query::new("EXEC sp_describe_undeclared_parameters @tsql = @p1");
            param_query.bind(sql.as_str());
            let param_count = match param_query.query(&mut self.inner.client).await {
                Ok(stream) => stream
                    .into_first_result()
                    .await
                    .map_err(tiberius_err)?
                    .len(),
                Err(e) => {
                    tracing::debug!("sp_describe_undeclared_parameters failed: {e}");
                    0
                }
            };

            Ok(crate::describe::Describe {
                parameters: Some(Either::Right(param_count)),
                columns,
                nullable,
            })
        })
    }
}

#[cfg(test)]
#[cfg(any(feature = "chrono", feature = "time"))]
mod tests {
    use super::*;

    #[test]
    fn days_since_epoch_zero() {
        assert_eq!(days_since_epoch_to_u32(0).unwrap(), 0);
    }

    #[test]
    fn days_since_epoch_max_date() {
        // 9999-12-31 is 3_652_058 days from 0001-01-01
        assert_eq!(days_since_epoch_to_u32(3_652_058).unwrap(), 3_652_058);
    }

    #[test]
    fn days_since_epoch_negative() {
        let err = days_since_epoch_to_u32(-1).unwrap_err();
        assert!(matches!(err, Error::Encode(_)));
    }

    #[test]
    fn days_since_epoch_overflow() {
        let err = days_since_epoch_to_u32(i64::from(MAX_DAYS) + 1).unwrap_err();
        assert!(matches!(err, Error::Encode(_)));
    }

    #[test]
    fn days_since_epoch_at_max() {
        assert_eq!(
            days_since_epoch_to_u32(i64::from(MAX_DAYS)).unwrap(),
            MAX_DAYS
        );
    }

    #[test]
    fn offset_minutes_zero() {
        assert_eq!(offset_minutes_to_i16(0).unwrap(), 0);
    }

    #[test]
    fn offset_minutes_positive_max() {
        assert_eq!(offset_minutes_to_i16(840).unwrap(), 840);
    }

    #[test]
    fn offset_minutes_negative_max() {
        assert_eq!(offset_minutes_to_i16(-840).unwrap(), -840);
    }

    #[test]
    fn offset_minutes_out_of_sql_range() {
        let err = offset_minutes_to_i16(841).unwrap_err();
        assert!(matches!(err, Error::Encode(_)));
        let err = offset_minutes_to_i16(-841).unwrap_err();
        assert!(matches!(err, Error::Encode(_)));
    }

    #[test]
    fn offset_minutes_i16_overflow() {
        let err = offset_minutes_to_i16(i32::MAX).unwrap_err();
        assert!(matches!(err, Error::Encode(_)));
    }
}

#[cfg(test)]
#[cfg(feature = "bigdecimal")]
mod bigdecimal_tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn positive_scale_simple() {
        // 123.45 → bigint=12345, exponent=2 → scale=2
        let bd = bigdecimal::BigDecimal::from_str("123.45").unwrap();
        let (value, scale) = bigdecimal_to_numeric(&bd).unwrap();
        assert_eq!(value, 12345);
        assert_eq!(scale, 2);
    }

    #[test]
    fn zero_scale() {
        // 42 → bigint=42, exponent=0 → scale=0
        let bd = bigdecimal::BigDecimal::from_str("42").unwrap();
        let (value, scale) = bigdecimal_to_numeric(&bd).unwrap();
        assert_eq!(value, 42);
        assert_eq!(scale, 0);
    }

    #[test]
    fn negative_exponent_rescales() {
        // Explicitly construct BigDecimal(123, -3) = 123 * 10^3 = 123000.
        // This is the internal form that triggers the negative-exponent path.
        let bd = bigdecimal::BigDecimal::new(123.into(), -3);
        let (bigint_raw, exp_raw) = bd.as_bigint_and_exponent();
        assert_eq!(exp_raw, -3, "precondition: exponent must be negative");
        assert_eq!(bigint_raw, 123.into(), "precondition: raw bigint is 123");

        let (value, scale) = bigdecimal_to_numeric(&bd).unwrap();
        // After rescaling: 123000 with scale 0
        assert_eq!(value, 123000);
        assert_eq!(scale, 0);
    }

    #[test]
    fn negative_exponent_large_magnitude() {
        // 5e10 = 50_000_000_000 → internally (5, -10)
        let bd = bigdecimal::BigDecimal::from_str("5e10").unwrap();
        let (value, scale) = bigdecimal_to_numeric(&bd).unwrap();
        assert_eq!(value, 50_000_000_000);
        assert_eq!(scale, 0);
    }

    #[test]
    fn scale_at_max_37() {
        // Scale exactly 37 is the maximum tiberius allows
        let bd = bigdecimal::BigDecimal::new(1.into(), 37);
        let (value, scale) = bigdecimal_to_numeric(&bd).unwrap();
        assert_eq!(value, 1);
        assert_eq!(scale, 37);
    }

    #[test]
    fn scale_38_rejected() {
        // Scale 38 triggers tiberius assert!(scale < 38); must be rejected
        let bd = bigdecimal::BigDecimal::new(1.into(), 38);
        let err = bigdecimal_to_numeric(&bd).unwrap_err();
        assert!(matches!(err, Error::Encode(_)));
    }

    #[test]
    fn scale_39_rejected() {
        let bd = bigdecimal::BigDecimal::new(1.into(), 39);
        let err = bigdecimal_to_numeric(&bd).unwrap_err();
        assert!(matches!(err, Error::Encode(_)));
    }

    #[test]
    fn scale_256_rejected_not_truncated() {
        // The original bug: `as u8` would silently truncate 256 → 0.
        // Must return an error, not scale=0.
        let bd = bigdecimal::BigDecimal::new(1.into(), 256);
        let err = bigdecimal_to_numeric(&bd).unwrap_err();
        assert!(matches!(err, Error::Encode(_)));
    }

    #[test]
    fn negative_value() {
        // -99.9 → bigint=-999, scale=1
        let bd = bigdecimal::BigDecimal::from_str("-99.9").unwrap();
        let (value, scale) = bigdecimal_to_numeric(&bd).unwrap();
        assert_eq!(value, -999);
        assert_eq!(scale, 1);
    }

    #[test]
    fn zero_value() {
        let bd = bigdecimal::BigDecimal::from_str("0").unwrap();
        let (value, scale) = bigdecimal_to_numeric(&bd).unwrap();
        assert_eq!(value, 0);
        assert_eq!(scale, 0);
    }
}
