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

struct TableColumnInfo {
    name: String,
    not_null: bool,
    dflt_value: Option<String>,
}

fn is_insert_statement(query: &str) -> bool {
    query.trim_start().to_uppercase().starts_with("INSERT")
}

fn extract_insert_info(query: &str) -> Option<(String, Option<Vec<String>>)> {
    // Parse simple INSERT statements to extract table name and optional column list
    // Handles: INSERT INTO table (col1, col2) VALUES ...
    //          INSERT INTO table VALUES ...
    // Returns: (table_name, Some(columns) or None for all columns)

    let trimmed = query.trim_start();
    let upper = trimmed.to_uppercase();

    if !upper.starts_with("INSERT INTO") {
        return None;
    }

    // Find table name after INSERT INTO
    let after_insert = upper.strip_prefix("INSERT INTO")?.trim_start();

    // Extract table name (handle backticks, quotes, brackets)
    let mut table_end = 0;
    let mut in_quote = false;
    let mut quote_char = ' ';
    let chars: Vec<char> = after_insert.chars().collect();

    for (i, &ch) in chars.iter().enumerate() {
        if !in_quote && (ch == '`' || ch == '"' || ch == '[') {
            in_quote = true;
            quote_char = if ch == '[' { ']' } else { ch };
            continue;
        }
        if in_quote && ch == quote_char {
            in_quote = false;
            table_end = i + 1;
            continue;
        }
        if !in_quote && (ch == ' ' || ch == '(') {
            table_end = i;
            break;
        }
        if !in_quote && (ch.is_whitespace() || ch == '(') {
            table_end = i;
            break;
        }
    }

    if table_end == 0 {
        table_end = after_insert.len();
    }

    let table_name_raw = after_insert[..table_end].trim();
    let table_name = table_name_raw
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('[')
        .trim_matches(']')
        .to_string();

    // Look for column list: TABLE (col1, col2)
    let remaining = after_insert[table_end..].trim_start();

    if remaining.starts_with('(') {
        // Find the matching closing paren
        let mut paren_depth = 0;
        let mut col_end = 0;
        for (i, ch) in remaining.chars().enumerate() {
            match ch {
                '(' => paren_depth += 1,
                ')' => {
                    paren_depth -= 1;
                    if paren_depth == 0 {
                        col_end = i;
                        break;
                    }
                }
                _ => {}
            }
        }

        if col_end > 1 {
            let potential_cols = &remaining[1..col_end];

            if potential_cols.contains(',') || !potential_cols.trim().is_empty() {
                // This looks like a column list
                let columns = potential_cols
                    .split(',')
                    .map(|c| {
                        c.trim()
                            .trim_matches('`')
                            .trim_matches('"')
                            .trim_matches('[')
                            .trim_matches(']')
                            .to_string()
                    })
                    .filter(|c| !c.is_empty())
                    .collect::<Vec<_>>();

                if !columns.is_empty() {
                    return Some((table_name, Some(columns)));
                }
            }
        }
    }

    // No columns specified - all columns are implied
    Some((table_name, None))
}

fn get_table_columns(
    conn: &mut ConnectionState,
    table_name: &str,
) -> Result<Vec<TableColumnInfo>, Error> {
    // PRAGMA table_info returns: cid, name, type, notnull, dflt_value, pk
    // Column indices: 0=cid, 1=name, 2=type, 3=notnull, 4=dflt_value, 5=pk
    let pragma_query = format!("PRAGMA table_info({})", table_name);

    let mut statement = match VirtualStatement::new(&pragma_query, false) {
        Ok(stmt) => stmt,
        Err(_) => return Ok(Vec::new()), // Skip validation if we can't prepare the PRAGMA
    };

    let mut columns = Vec::new();

    while let Some(stmt) = statement.prepare_next(&mut conn.handle)? {
        // Step through results
        while stmt.handle.step()? {
            // Get column name - safe since column_text can't fail if step succeeded
            let name = match stmt.handle.column_text(1) {
                Ok(n) => n.to_string(),
                Err(_) => continue,
            };

            // Get notnull flag
            let not_null = stmt.handle.column_int(3) != 0;

            // Get default value
            let dflt_value = match stmt.handle.column_text(4) {
                Ok(v) => Some(v.to_string()),
                Err(_) => None,
            };

            columns.push(TableColumnInfo {
                name,
                not_null,
                dflt_value,
            });
        }
    }

    Ok(columns)
}

fn validate_insert_statement(conn: &mut ConnectionState, query: &str) -> Result<(), Error> {
    // Extract table name and specified columns from INSERT
    let (table_name, specified_cols_opt) = match extract_insert_info(query) {
        Some(info) => info,
        None => return Ok(()), // Skip validation for queries we can't parse
    };

    // Get table schema
    let all_columns = match get_table_columns(conn, &table_name) {
        Ok(cols) => cols,
        Err(_) => return Ok(()), // Table doesn't exist or error querying schema - skip validation
    };

    // Find NOT NULL columns without defaults
    let required_cols = all_columns
        .iter()
        .filter(|col| col.not_null && col.dflt_value.is_none())
        .collect::<Vec<_>>();

    // If specific columns were listed, validate they include all required columns
    if let Some(ref specified_cols) = specified_cols_opt {
        let specified_upper = specified_cols
            .iter()
            .map(|c| c.to_uppercase())
            .collect::<Vec<_>>();

        let missing = required_cols
            .iter()
            .filter(|col| !specified_upper.contains(&col.name.to_uppercase()))
            .collect::<Vec<_>>();

        if !missing.is_empty() {
            let missing_names = missing
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::Configuration(
                format!(
                    "INSERT into {} missing NOT NULL column(s) without defaults: {}",
                    table_name, missing_names
                )
                .into(),
            ));
        }
    }
    // If no specific columns listed, VALUES (...) implies all columns in table order
    // SQLite will validate this at runtime, so we skip compile-time validation here

    Ok(())
}

pub(crate) fn describe(
    conn: &mut ConnectionState,
    query: SqlStr,
) -> Result<Describe<Sqlite>, Error> {
    // describing a statement from SQLite can be involved
    // each SQLx statement is comprised of multiple SQL statements

    // Validate INSERT statements for NOT NULL constraint completeness
    if is_insert_statement(query.as_str()) {
        validate_insert_statement(conn, query.as_str())?;
    }

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

            nullable.push(exp_nullable.or(col_nullable));

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
