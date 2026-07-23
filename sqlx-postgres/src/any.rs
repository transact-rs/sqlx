use crate::{
    Either, PgColumn, PgConnectOptions, PgConnection, PgQueryResult, PgRow, PgTransactionManager,
    PgTypeInfo, Postgres,
};
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_util::{stream, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use sqlx_core::sql_str::{AssertSqlSafe, SqlSafeStr, SqlStr};
use std::borrow::Cow;
use std::{future, pin::pin};

use sqlx_core::any::{
    AnyArguments, AnyColumn, AnyConnectOptions, AnyConnectionBackend, AnyQueryResult, AnyRow,
    AnyStatement, AnyTypeInfo, AnyTypeInfoKind,
};

use crate::type_info::PgType;
use sqlx_core::connection::Connection;
use sqlx_core::database::Database;
use sqlx_core::executor::Executor;
use sqlx_core::ext::ustr::UStr;
use sqlx_core::transaction::TransactionManager;

sqlx_core::declare_driver_with_optional_migrate!(DRIVER = Postgres);

/// Rewrite `?`-style placeholders (as produced by the `Any` driver, e.g. via `QueryBuilder<Any>`)
/// into Postgres-style positional placeholders (`$1`, `$2`, ...).
///
/// `AnyArguments` binds its values in the same left-to-right order that `?` placeholders were
/// written, so numbering them `1..=N` in order of appearance preserves the correct binding.
///
/// This is SQL-aware to avoid rewriting a literal `?` character that appears inside:
/// - a single-quoted string literal (`'...'`, with `''` as an escaped quote)
/// - a double-quoted identifier (`"..."`, with `""` as an escaped quote)
/// - a dollar-quoted string (`$$...$$` or `$tag$...$tag$`)
/// - a `--` line comment or a `/* */` block comment (not nested)
///
/// Only MySQL and SQLite use `?` natively, so this rewrite is only needed on the Postgres leg
/// of the `Any` driver; see <https://github.com/transact-rs/sqlx/issues/3000>.
fn rewrite_any_placeholders(sql: &str) -> Cow<'_, str> {
    if !sql.contains('?') {
        return Cow::Borrowed(sql);
    }

    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut i = 0usize;
    let mut placeholder_num = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '?' => {
                placeholder_num += 1;
                out.push('$');
                out.push_str(&placeholder_num.to_string());
                i += 1;
            }
            '\'' | '"' => {
                // String literal or quoted identifier; the doubled-quote is the escape for both.
                let quote = c;
                out.push(c);
                i += 1;
                while i < chars.len() {
                    let ch = chars[i];
                    out.push(ch);
                    i += 1;
                    if ch == quote {
                        if chars.get(i) == Some(&quote) {
                            out.push(quote);
                            i += 1;
                            continue;
                        }
                        break;
                    }
                }
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                // Line comment: copy through to (but not including) the newline.
                while i < chars.len() && chars[i] != '\n' {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                // Block comment (not handling nesting; Postgres nested comments are a rare
                // edge case and out of scope for this fix).
                out.push(chars[i]);
                out.push(chars[i + 1]);
                i += 2;
                while i < chars.len() {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        out.push('*');
                        out.push('/');
                        i += 2;
                        break;
                    }
                    out.push(chars[i]);
                    i += 1;
                }
            }
            '$' => {
                // Possible dollar-quote opening tag: `$tag$` where `tag` is `[A-Za-z0-9_]*`
                // (empty tag is the common `$$...$$` form).
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '$' {
                    let tag: String = chars[i + 1..j].iter().collect();
                    let open: String = chars[i..=j].iter().collect();
                    out.push_str(&open);
                    i = j + 1;

                    let close: Vec<char> = format!("${tag}$").chars().collect();
                    let mut found = false;
                    while i < chars.len() {
                        if chars[i..].starts_with(&close[..]) {
                            out.extend(&close);
                            i += close.len();
                            found = true;
                            break;
                        }
                        out.push(chars[i]);
                        i += 1;
                    }
                    // If unterminated, we've already copied through to the end of input;
                    // nothing further to do (matches Postgres's own eventual parse error).
                    let _ = found;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }

    Cow::Owned(out)
}

/// Apply [`rewrite_any_placeholders`] to a [`SqlStr`], only allocating a new one if a rewrite
/// was actually needed.
fn maybe_rewrite_any_placeholders(sql: SqlStr) -> SqlStr {
    match rewrite_any_placeholders(sql.as_str()) {
        Cow::Borrowed(_) => sql,
        Cow::Owned(rewritten) => AssertSqlSafe(rewritten).into_sql_str(),
    }
}

impl AnyConnectionBackend for PgConnection {
    fn name(&self) -> &str {
        <Postgres as Database>::NAME
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
        PgTransactionManager::begin(self, statement).boxed()
    }

    fn commit(&mut self) -> BoxFuture<'_, sqlx_core::Result<()>> {
        PgTransactionManager::commit(self).boxed()
    }

    fn rollback(&mut self) -> BoxFuture<'_, sqlx_core::Result<()>> {
        PgTransactionManager::rollback(self).boxed()
    }

    fn start_rollback(&mut self) {
        PgTransactionManager::start_rollback(self)
    }

    fn get_transaction_depth(&self) -> usize {
        PgTransactionManager::get_transaction_depth(self)
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
        let query = maybe_rewrite_any_placeholders(query);
        let persistent = persistent && arguments.is_some();
        let arguments = match arguments.map(AnyArguments::convert_into).transpose() {
            Ok(arguments) => arguments,
            Err(error) => {
                return stream::once(future::ready(Err(sqlx_core::Error::Encode(error)))).boxed()
            }
        };

        Box::pin(
            self.run(query, arguments, persistent, None)
                .try_flatten_stream()
                .map(
                    move |res: sqlx_core::Result<Either<PgQueryResult, PgRow>>| match res? {
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
        let query = maybe_rewrite_any_placeholders(query);
        let persistent = persistent && arguments.is_some();
        let arguments = arguments
            .map(AnyArguments::convert_into)
            .transpose()
            .map_err(sqlx_core::Error::Encode);

        Box::pin(async move {
            let arguments = arguments?;
            let mut stream = pin!(self.run(query, arguments, persistent, None).await?);

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
        let sql = maybe_rewrite_any_placeholders(sql);
        Box::pin(async move {
            let statement = Executor::prepare_with(self, sql, &[]).await?;
            let column_names = statement.metadata.column_names.clone();
            AnyStatement::try_from_statement(statement, column_names)
        })
    }

    #[cfg(feature = "offline")]
    fn describe<'c>(
        &mut self,
        sql: SqlStr,
    ) -> BoxFuture<'_, sqlx_core::Result<sqlx_core::describe::Describe<sqlx_core::any::Any>>> {
        let sql = maybe_rewrite_any_placeholders(sql);
        Box::pin(async move {
            let describe = Executor::describe(self, sql).await?;

            let columns = describe
                .columns
                .iter()
                .map(AnyColumn::try_from)
                .collect::<Result<Vec<_>, _>>()?;

            let parameters = match describe.parameters {
                Some(Either::Left(parameters)) => Some(Either::Left(
                    parameters
                        .iter()
                        .enumerate()
                        .map(|(i, type_info)| {
                            AnyTypeInfo::try_from(type_info).map_err(|_| {
                                sqlx_core::Error::AnyDriverError(
                                    format!(
                                        "Any driver does not support type {type_info} of parameter {i}"
                                    )
                                    .into(),
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                )),
                Some(Either::Right(count)) => Some(Either::Right(count)),
                None => None,
            };

            Ok(sqlx_core::describe::Describe {
                columns,
                parameters,
                nullable: describe.nullable,
            })
        })
    }
}

impl<'a> TryFrom<&'a PgTypeInfo> for AnyTypeInfo {
    type Error = sqlx_core::Error;

    fn try_from(pg_type: &'a PgTypeInfo) -> Result<Self, Self::Error> {
        Ok(AnyTypeInfo {
            kind: match &pg_type.0 {
                PgType::Bool => AnyTypeInfoKind::Bool,
                PgType::Void => AnyTypeInfoKind::Null,
                PgType::Int2 => AnyTypeInfoKind::SmallInt,
                PgType::Int4 => AnyTypeInfoKind::Integer,
                PgType::Int8 => AnyTypeInfoKind::BigInt,
                PgType::Float4 => AnyTypeInfoKind::Real,
                PgType::Float8 => AnyTypeInfoKind::Double,
                PgType::Bytea => AnyTypeInfoKind::Blob,
                PgType::Text | PgType::Varchar => AnyTypeInfoKind::Text,
                PgType::DeclareWithName(UStr::Static("citext")) => AnyTypeInfoKind::Text,
                _ => {
                    return Err(sqlx_core::Error::AnyDriverError(
                        format!("Any driver does not support the Postgres type {pg_type:?}").into(),
                    ))
                }
            },
        })
    }
}

impl<'a> TryFrom<&'a PgColumn> for AnyColumn {
    type Error = sqlx_core::Error;

    fn try_from(col: &'a PgColumn) -> Result<Self, Self::Error> {
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

impl<'a> TryFrom<&'a PgRow> for AnyRow {
    type Error = sqlx_core::Error;

    fn try_from(row: &'a PgRow) -> Result<Self, Self::Error> {
        AnyRow::map_from(row, row.metadata.column_names.clone())
    }
}

impl<'a> TryFrom<&'a AnyConnectOptions> for PgConnectOptions {
    type Error = sqlx_core::Error;

    fn try_from(value: &'a AnyConnectOptions) -> Result<Self, Self::Error> {
        let mut opts = PgConnectOptions::parse_from_url(&value.database_url)?;
        opts.log_settings = value.log_settings.clone();
        Ok(opts)
    }
}

fn map_result(res: PgQueryResult) -> AnyQueryResult {
    AnyQueryResult {
        rows_affected: res.rows_affected(),
        last_insert_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::rewrite_any_placeholders;

    #[test]
    fn no_placeholders_is_borrowed_unchanged() {
        let sql = "SELECT * FROM foo";
        assert!(matches!(
            rewrite_any_placeholders(sql),
            std::borrow::Cow::Borrowed(s) if s == sql
        ));
    }

    #[test]
    fn simple_placeholders_are_numbered_in_order() {
        assert_eq!(
            rewrite_any_placeholders("SELECT * FROM foo WHERE a = ? AND b = ?"),
            "SELECT * FROM foo WHERE a = $1 AND b = $2"
        );
    }

    #[test]
    fn limit_offset_placeholders() {
        assert_eq!(
            rewrite_any_placeholders("SELECT * FROM foo LIMIT ? OFFSET ?"),
            "SELECT * FROM foo LIMIT $1 OFFSET $2"
        );
    }

    #[test]
    fn question_mark_in_string_literal_is_untouched() {
        assert_eq!(
            rewrite_any_placeholders("SELECT * FROM foo WHERE q = 'is this ok?' AND a = ?"),
            "SELECT * FROM foo WHERE q = 'is this ok?' AND a = $1"
        );
    }

    #[test]
    fn escaped_quote_in_string_literal() {
        assert_eq!(
            rewrite_any_placeholders("SELECT 'it''s a ? test' WHERE a = ?"),
            "SELECT 'it''s a ? test' WHERE a = $1"
        );
    }

    #[test]
    fn question_mark_in_quoted_identifier_is_untouched() {
        assert_eq!(
            rewrite_any_placeholders(r#"SELECT "weird?col" FROM foo WHERE a = ?"#),
            r#"SELECT "weird?col" FROM foo WHERE a = $1"#
        );
    }

    #[test]
    fn question_mark_in_dollar_quoted_string_is_untouched() {
        assert_eq!(
            rewrite_any_placeholders("SELECT $$has a ? in it$$ WHERE a = ?"),
            "SELECT $$has a ? in it$$ WHERE a = $1"
        );
    }

    #[test]
    fn question_mark_in_tagged_dollar_quoted_string_is_untouched() {
        assert_eq!(
            rewrite_any_placeholders("SELECT $tag$has a ? in it$tag$ WHERE a = ?"),
            "SELECT $tag$has a ? in it$tag$ WHERE a = $1"
        );
    }

    #[test]
    fn question_mark_in_line_comment_is_untouched() {
        assert_eq!(
            rewrite_any_placeholders("SELECT a -- is this ok?\nWHERE a = ?"),
            "SELECT a -- is this ok?\nWHERE a = $1"
        );
    }

    #[test]
    fn question_mark_in_block_comment_is_untouched() {
        assert_eq!(
            rewrite_any_placeholders("SELECT a /* ok? */ WHERE a = ?"),
            "SELECT a /* ok? */ WHERE a = $1"
        );
    }
}
