use crate::connection::explain::explain;
use crate::connection::ConnectionState;
use crate::describe::Describe;
use crate::error::Error;
use crate::statement::VirtualStatement;
use crate::type_info::DataType;
use crate::{Sqlite, SqliteColumn};
use sqlx_core::sql_str::SqlStr;
use sqlx_core::Either;
use std::convert::identity;

pub(crate) fn describe(
    conn: &mut ConnectionState,
    query: SqlStr,
) -> Result<Describe<Sqlite>, Error> {
    // describing a statement from SQLite can be involved
    // each SQLx statement is comprised of multiple SQL statements

    let mut statement = VirtualStatement::new(query.as_str(), false)?;

    let mut columns = Vec::new();
    let mut nullable = Vec::new();
    let mut num_params = 0;

    // we start by finding the first statement that *can* return results
    while let Some(stmt) = statement.prepare_next(&mut conn.handle)? {
        num_params += stmt.handle.bind_parameter_count();

        let mut stepped = false;

        let num = stmt.handle.column_count();
        if num == 0 {
            // no columns in this statement; skip
            continue;
        }

        // next we try to use [column_decltype] to inspect the type of each column
        columns.reserve(num);

        // as a last resort, we explain the original query and attempt to
        // infer what would the expression types be as a fallback
        // to [column_decltype]

        // if explain.. fails, ignore the failure and we'll have no fallback
        let (fallback, fallback_nullable) = match explain(conn, stmt.handle.sql()) {
            Ok(v) => v,
            Err(error) => {
                tracing::debug!(%error, "describe: explain introspection failed");

                (vec![], vec![])
            }
        };

        for col in 0..num {
            let name = stmt.handle.column_name(col).to_owned();

            let origin = stmt.handle.column_origin(col);

            let type_info = if let Some(ty) = stmt.handle.column_decltype(col) {
                ty
            } else {
                // if that fails, we back up and attempt to step the statement
                // once *if* its read-only and then use [column_type] as a
                // fallback to [column_decltype]
                if !stepped && stmt.handle.read_only() {
                    stepped = true;
                    let _ = stmt.handle.step();
                }

                let mut ty = stmt.handle.column_type_info(col);

                if ty.0 == DataType::Null {
                    if let Some(fallback) = fallback.get(col).cloned() {
                        ty = fallback;
                    }
                }

                ty
            };

            // check explain
            let col_nullable = stmt.handle.column_nullable(col)?;
            let exp_nullable = fallback_nullable.get(col).copied().and_then(identity);

            // If the column has a known schema origin that says NOT NULL,
            // trust that over the explain analysis which may lose NOT NULL
            // constraints through ephemeral tables / sorters (e.g. ORDER BY).
            // See: https://github.com/launchbadge/sqlx/issues/4147
            let result_nullable = match (col_nullable, exp_nullable) {
                // Schema says NOT NULL — trust it regardless of explain result
                (Some(false), _) => Some(false),
                // Schema doesn't know (e.g. expression column), use explain
                (None, exp) => exp,
                // Both agree or only schema has info
                (col, None) => col,
                // Schema says nullable, explain says not — be conservative, say nullable
                (Some(true), Some(false)) => Some(true),
                // Both say nullable
                (Some(true), Some(true)) => Some(true),
            };
            nullable.push(result_nullable);

            columns.push(SqliteColumn {
                name: name.into(),
                type_info,
                ordinal: col,
                origin,
            });
        }
    }

    Ok(Describe {
        columns,
        parameters: Some(Either::Right(num_params)),
        nullable,
    })
}
