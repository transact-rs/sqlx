use futures_util::TryStreamExt;
use sqlx::mssql::MssqlRow;
use sqlx::mssql::{Mssql, MssqlPoolOptions};
use sqlx::mssql::{MssqlAdvisoryLock, MssqlIsolationLevel};
use sqlx::{
    AssertSqlSafe, Column, Connection, Either, Executor, MssqlConnection, Row, SqlSafeStr,
    Statement, TypeInfo,
};
use sqlx_test::new;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

#[sqlx_macros::test]
async fn it_connects() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    conn.ping().await?;

    conn.close().await?;

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_select_expression() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    let row: MssqlRow = conn.fetch_one("SELECT 4").await?;
    let v: i32 = row.try_get(0)?;

    assert_eq!(v, 4);

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_select_expression_by_name() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    let row: MssqlRow = conn.fetch_one("SELECT 4 as _3").await?;
    let v: i32 = row.try_get("_3")?;

    assert_eq!(v, 4);

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_fail_to_connect() -> anyhow::Result<()> {
    let mut url = dotenvy::var("DATABASE_URL")?;
    url = url.replace("Password", "NotPassword");

    let res = MssqlConnection::connect(&url).await;
    let err = res.unwrap_err();
    let err = err.into_database_error().unwrap();

    assert_eq!(err.message(), "Login failed for user \'sa\'.");

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_inspect_errors() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    let res: Result<_, sqlx::Error> = sqlx::query("select f").execute(&mut conn).await;
    let err = res.unwrap_err();

    // can also do [as_database_error] or use `match ..`
    let err = err.into_database_error().unwrap();

    assert_eq!(err.message(), "Invalid column name 'f'.");

    Ok(())
}

#[sqlx_macros::test]
async fn it_maths() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    let value = sqlx::query("SELECT 1 + @p1")
        .bind(5_i32)
        .try_map(|row: MssqlRow| row.try_get::<i32, _>(0))
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(6_i32, value);

    Ok(())
}

#[sqlx_macros::test]
async fn it_executes() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    let _ = conn
        .execute(
            r#"
CREATE TABLE #users (id INTEGER PRIMARY KEY);
            "#,
        )
        .await?;

    for index in 1..=10_i32 {
        let done = sqlx::query("INSERT INTO #users (id) VALUES (@p1)")
            .bind(index * 2)
            .execute(&mut conn)
            .await?;

        assert_eq!(done.rows_affected(), 1);
    }

    let sum: i32 = sqlx::query("SELECT id FROM #users")
        .try_map(|row: MssqlRow| row.try_get::<i32, _>(0))
        .fetch(&mut conn)
        .try_fold(0_i32, |acc, x| async move { Ok(acc + x) })
        .await?;

    assert_eq!(sum, 110);

    Ok(())
}

// Regression test: parameterized INSERT/UPDATE/DELETE must report the real
// affected-row count from SQL Server's DONE token, not 0. The execute path goes
// through tiberius `execute()` (ExecuteResult) rather than the row-counting
// fetch path.
#[sqlx_macros::test]
async fn it_reports_rows_affected_for_dml() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    conn.execute("CREATE TABLE #widgets (id INTEGER PRIMARY KEY, label NVARCHAR(50) NULL);")
        .await?;

    for index in 1..=5_i32 {
        let done = sqlx::query("INSERT INTO #widgets (id) VALUES (@p1)")
            .bind(index)
            .execute(&mut conn)
            .await?;
        assert_eq!(done.rows_affected(), 1);
    }

    // UPDATE that hits exactly one row.
    let done = sqlx::query("UPDATE #widgets SET label = @p1 WHERE id = @p2")
        .bind("hit")
        .bind(3_i32)
        .execute(&mut conn)
        .await?;
    assert_eq!(done.rows_affected(), 1);

    // UPDATE that matches no rows -> 0. This proves the count is real, not just
    // "always non-zero".
    let done = sqlx::query("UPDATE #widgets SET label = @p1 WHERE id = @p2")
        .bind("miss")
        .bind(999_i32)
        .execute(&mut conn)
        .await?;
    assert_eq!(done.rows_affected(), 0);

    // Multi-row DELETE reports the number of rows removed.
    let done = sqlx::query("DELETE FROM #widgets WHERE id >= @p1")
        .bind(3_i32)
        .execute(&mut conn)
        .await?;
    assert_eq!(done.rows_affected(), 3);

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_return_1000_rows() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    let _ = conn
        .execute(
            r#"
CREATE TABLE #users (id INTEGER PRIMARY KEY);
            "#,
        )
        .await?;

    for index in 1..=1000_i32 {
        let done = sqlx::query("INSERT INTO #users (id) VALUES (@p1)")
            .bind(index * 2)
            .execute(&mut conn)
            .await?;

        assert_eq!(done.rows_affected(), 1);
    }

    let sum: i32 = sqlx::query("SELECT id FROM #users")
        .try_map(|row: MssqlRow| row.try_get::<i32, _>(0))
        .fetch(&mut conn)
        .try_fold(0_i32, |acc, x| async move { Ok(acc + x) })
        .await?;

    assert_eq!(sum, 1001000);

    Ok(())
}

#[sqlx_macros::test]
async fn it_selects_null() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    let (_, val): (i32, Option<i32>) = sqlx::query_as("SELECT 5, NULL")
        .fetch_one(&mut conn)
        .await?;

    assert!(val.is_none());

    let val: Option<i32> = conn.fetch_one("SELECT 10, NULL").await?.try_get(1)?;

    assert!(val.is_none());

    Ok(())
}

#[sqlx_macros::test]
async fn it_binds_empty_string_and_null() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    let (val, val2): (String, Option<String>) = sqlx::query_as("SELECT @p1, @p2")
        .bind("")
        .bind(None::<String>)
        .fetch_one(&mut conn)
        .await?;

    assert!(val.is_empty());
    assert!(val2.is_none());

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_work_with_transactions() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    conn.execute("IF OBJECT_ID('_sqlx_users_1922', 'U') IS NULL CREATE TABLE _sqlx_users_1922 (id INTEGER PRIMARY KEY)")
        .await?;

    conn.execute("DELETE FROM _sqlx_users_1922").await?;

    // begin .. rollback

    let mut tx = conn.begin().await?;

    sqlx::query("INSERT INTO _sqlx_users_1922 (id) VALUES (@p1)")
        .bind(10_i32)
        .execute(&mut *tx)
        .await?;

    tx.rollback().await?;

    let (count,): (i32,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_users_1922")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(count, 0);

    // begin .. commit

    let mut tx = conn.begin().await?;

    sqlx::query("INSERT INTO _sqlx_users_1922 (id) VALUES (@p1)")
        .bind(10_i32)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    let (count,): (i32,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_users_1922")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(count, 1);

    // begin .. (drop)

    {
        let mut tx = conn.begin().await?;

        sqlx::query("INSERT INTO _sqlx_users_1922 (id) VALUES (@p1)")
            .bind(20_i32)
            .execute(&mut *tx)
            .await?;
    }

    conn = new::<Mssql>().await?;

    let (count,): (i32,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_users_1922")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(count, 1);

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_work_with_nested_transactions() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    conn.execute("IF OBJECT_ID('_sqlx_users_2523', 'U') IS NULL CREATE TABLE _sqlx_users_2523 (id INTEGER PRIMARY KEY)")
        .await?;

    conn.execute("DELETE FROM _sqlx_users_2523").await?;

    // begin
    let mut tx = conn.begin().await?;

    // insert a user
    sqlx::query("INSERT INTO _sqlx_users_2523 (id) VALUES (@p1)")
        .bind(50_i32)
        .execute(&mut *tx)
        .await?;

    // begin once more
    let mut tx2 = tx.begin().await?;

    // insert another user
    sqlx::query("INSERT INTO _sqlx_users_2523 (id) VALUES (@p1)")
        .bind(10_i32)
        .execute(&mut *tx2)
        .await?;

    // never mind, rollback
    tx2.rollback().await?;

    // did we really?
    let (count,): (i32,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_users_2523")
        .fetch_one(&mut *tx)
        .await?;

    assert_eq!(count, 1);

    // actually, commit
    tx.commit().await?;

    // did we really?
    let (count,): (i32,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_users_2523")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(count, 1);

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_prepare_then_execute() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;
    let mut tx = conn.begin().await?;

    let tweet_id: i64 = sqlx::query_scalar(
        "INSERT INTO tweet ( id, text ) OUTPUT INSERTED.id VALUES ( 50, 'Hello, World' )",
    )
    .fetch_one(&mut *tx)
    .await?;

    let statement = tx
        .prepare("SELECT * FROM tweet WHERE id = @p1".into_sql_str())
        .await?;

    assert_eq!(statement.column(0).name(), "id");
    assert_eq!(statement.column(1).name(), "text");
    assert_eq!(statement.column(2).name(), "is_sent");
    assert_eq!(statement.column(3).name(), "owner_id");

    assert_eq!(statement.column(0).type_info().name(), "BIGINT");
    assert_eq!(statement.column(1).type_info().name(), "NVARCHAR");
    assert_eq!(statement.column(2).type_info().name(), "TINYINT");
    assert_eq!(statement.column(3).type_info().name(), "BIGINT");

    let row = statement.query().bind(tweet_id).fetch_one(&mut *tx).await?;
    let tweet_text: String = row.try_get("text")?;

    assert_eq!(tweet_text, "Hello, World");

    Ok(())
}

// MSSQL-specific copy of the test case in `tests/any/pool.rs`
// because MSSQL has its own bespoke syntax for temporary tables.
#[sqlx_macros::test]
async fn test_pool_callbacks() -> anyhow::Result<()> {
    #[derive(sqlx::FromRow, Debug, PartialEq, Eq)]
    struct ConnStats {
        id: i32,
        before_acquire_calls: i32,
        after_release_calls: i32,
    }

    sqlx_test::setup_if_needed();

    let current_id = AtomicI32::new(0);

    let pool = MssqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, meta| {
            assert_eq!(meta.age, Duration::ZERO);
            assert_eq!(meta.idle_for, Duration::ZERO);

            let id = current_id.fetch_add(1, Ordering::AcqRel);

            Box::pin(async move {
                let statement = format!(
                    // language=MSSQL
                    r#"
                    CREATE TABLE #conn_stats(
                        id int primary key,
                        before_acquire_calls int default 0,
                        after_release_calls int default 0
                    );
                    INSERT INTO #conn_stats(id) VALUES ({});
                    "#,
                    // Until we have generalized bind parameters
                    id
                );

                conn.execute(sqlx::AssertSqlSafe(statement)).await?;
                Ok(())
            })
        })
        .before_acquire(|conn, meta| {
            // `age` and `idle_for` should both be nonzero
            assert_ne!(meta.age, Duration::ZERO);
            assert_ne!(meta.idle_for, Duration::ZERO);

            Box::pin(async move {
                // MSSQL doesn't support UPDATE ... RETURNING either
                sqlx::query(
                    r#"
                        UPDATE #conn_stats
                        SET before_acquire_calls = before_acquire_calls + 1
                    "#,
                )
                .execute(&mut *conn)
                .await?;

                let stats: ConnStats = sqlx::query_as("SELECT * FROM #conn_stats")
                    .fetch_one(conn)
                    .await?;

                // For even IDs, cap by the number of before_acquire calls.
                // Ignore the check for odd IDs.
                Ok((stats.id & 1) == 1 || stats.before_acquire_calls < 3)
            })
        })
        .after_release(|conn, meta| {
            // `age` should be nonzero but `idle_for` should be zero.
            assert_ne!(meta.age, Duration::ZERO);
            assert_eq!(meta.idle_for, Duration::ZERO);

            Box::pin(async move {
                sqlx::query(
                    r#"
                        UPDATE #conn_stats
                        SET after_release_calls = after_release_calls + 1
                    "#,
                )
                .execute(&mut *conn)
                .await?;

                let stats: ConnStats = sqlx::query_as("SELECT * FROM #conn_stats")
                    .fetch_one(conn)
                    .await?;

                // For odd IDs, cap by the number of before_release calls.
                // Ignore the check for even IDs.
                Ok((stats.id & 1) == 0 || stats.after_release_calls < 4)
            })
        })
        // Don't establish a connection yet.
        .connect_lazy(&std::env::var("DATABASE_URL")?)?;

    // Expected pattern of (id, before_acquire_calls, after_release_calls)
    let pattern = [
        // The connection pool starts empty.
        (0, 0, 0),
        (0, 1, 1),
        (0, 2, 2),
        (1, 0, 0),
        (1, 1, 1),
        (1, 2, 2),
        // We should expect one more `acquire` because the ID is odd
        (1, 3, 3),
        (2, 0, 0),
        (2, 1, 1),
        (2, 2, 2),
        (3, 0, 0),
    ];

    for (id, before_acquire_calls, after_release_calls) in pattern {
        let conn_stats: ConnStats = sqlx::query_as("SELECT * FROM #conn_stats")
            .fetch_one(&pool)
            .await?;

        assert_eq!(
            conn_stats,
            ConnStats {
                id,
                before_acquire_calls,
                after_release_calls
            }
        );
    }

    pool.close().await;

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_query_multiple_result_sets() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    // A batch that produces two result sets. `fetch_many` streams the rows from
    // every result set in order, interleaved with `Either::Left` query-result
    // markers.
    let results: Vec<Either<_, _>> = (&mut conn)
        .fetch_many("SELECT 1 AS a; SELECT 2 AS b, 3 AS c;")
        .try_collect()
        .await?;

    let rows: Vec<_> = results
        .iter()
        .filter_map(|item| item.as_ref().right())
        .collect();

    // First result set: one row with column "a".
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].try_get::<i32, _>("a")?, 1);

    // Second result set: one row with columns "b" and "c".
    assert_eq!(rows[1].try_get::<i32, _>("b")?, 2);
    assert_eq!(rows[1].try_get::<i32, _>("c")?, 3);

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_inspect_column_metadata() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    let statement = conn
        .prepare("SELECT CAST(1 AS INT) AS int_col, CAST('hello' AS NVARCHAR(50)) AS str_col, CAST(NULL AS BIGINT) AS nullable_col".into_sql_str())
        .await?;

    assert_eq!(statement.column(0).name(), "int_col");
    assert_eq!(statement.column(1).name(), "str_col");
    assert_eq!(statement.column(2).name(), "nullable_col");

    assert_eq!(statement.column(0).type_info().name(), "INT");
    // sp_describe_first_result_set returns "NVARCHAR(50)" for typed NVARCHAR
    assert!(statement
        .column(1)
        .type_info()
        .name()
        .starts_with("NVARCHAR"));
    assert_eq!(statement.column(2).type_info().name(), "BIGINT");

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_reuse_connection_after_error() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    // Cause an error
    let res: Result<_, sqlx::Error> = sqlx::query("SELECT * FROM this_table_does_not_exist_12345")
        .execute(&mut conn)
        .await;
    assert!(res.is_err());

    // Connection should still be usable
    let val: (i32,) = sqlx::query_as("SELECT 42").fetch_one(&mut conn).await?;
    assert_eq!(val.0, 42);

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_bind_many_parameters() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    // Build a query with 100 parameters: SELECT @p1 + @p2 + ... + @p100
    let param_refs: Vec<String> = (1..=100).map(|i| format!("@p{i}")).collect();
    let sql = format!("SELECT {}", param_refs.join(" + "));

    let mut query = sqlx::query_scalar::<_, i32>(AssertSqlSafe(sql));
    for _ in 0..100 {
        query = query.bind(1_i32);
    }

    let result: i32 = query.fetch_one(&mut conn).await?;
    assert_eq!(result, 100);

    Ok(())
}

#[sqlx_macros::test]
async fn it_handles_special_characters_in_strings() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    // Single quotes
    let val: (String,) = sqlx::query_as("SELECT @p1")
        .bind("it's a test")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(val.0, "it's a test");

    // Backslashes
    let val: (String,) = sqlx::query_as("SELECT @p1")
        .bind(r"C:\Users\test")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(val.0, r"C:\Users\test");

    // Unicode
    let val: (String,) = sqlx::query_as("SELECT @p1")
        .bind("\u{1F600} hello \u{4E16}\u{754C}")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(val.0, "\u{1F600} hello \u{4E16}\u{754C}");

    // Newlines and tabs
    let val: (String,) = sqlx::query_as("SELECT @p1")
        .bind("line1\nline2\ttab")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(val.0, "line1\nline2\ttab");

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_use_transaction_isolation_levels() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    // Start a transaction with READ UNCOMMITTED isolation
    let mut tx = conn
        .begin_with_isolation(MssqlIsolationLevel::ReadUncommitted)
        .await?;

    // Verify we can do work inside the transaction
    let val: (i32,) = sqlx::query_as("SELECT 1").fetch_one(&mut *tx).await?;
    assert_eq!(val.0, 1);

    tx.commit().await?;

    // Start a transaction with SERIALIZABLE isolation
    let mut tx = conn
        .begin_with_isolation(MssqlIsolationLevel::Serializable)
        .await?;

    let val: (i32,) = sqlx::query_as("SELECT 2").fetch_one(&mut *tx).await?;
    assert_eq!(val.0, 2);

    tx.rollback().await?;

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_use_advisory_lock_guard() -> anyhow::Result<()> {
    let mut conn = new::<Mssql>().await?;

    // Need a transaction context for sp_getapplock with Session owner
    // Actually, Session-scoped locks work outside transactions too.
    let lock = MssqlAdvisoryLock::new("sqlx_test_lock_guard");

    // Acquire the lock via the RAII guard
    let mut guard = lock.acquire_guard(&mut conn).await?;

    // Use the connection through the guard
    let val: (i32,) = sqlx::query_as("SELECT 99").fetch_one(&mut *guard).await?;
    assert_eq!(val.0, 99);

    // Release the lock and get the connection back
    let conn = guard.release_now().await?;

    // Verify we can still use the connection
    let val: (i32,) = sqlx::query_as("SELECT 100").fetch_one(conn).await?;
    assert_eq!(val.0, 100);

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_try_acquire_advisory_lock() -> anyhow::Result<()> {
    let mut conn1 = new::<Mssql>().await?;
    let mut conn2 = new::<Mssql>().await?;

    let lock = MssqlAdvisoryLock::new("sqlx_test_try_lock");

    // Acquire on conn1
    lock.acquire(&mut conn1).await?;

    // Try to acquire on conn2 — should fail (return false) since it's exclusive
    let acquired = lock.try_acquire(&mut conn2).await?;
    assert!(!acquired);

    // Release on conn1
    let released = lock.release(&mut conn1).await?;
    assert!(released);

    // Now conn2 should be able to acquire
    let acquired = lock.try_acquire(&mut conn2).await?;
    assert!(acquired);

    lock.release(&mut conn2).await?;

    Ok(())
}
