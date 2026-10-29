use anyhow::Context;
use sqlx::{
    error::ErrorKind,
    mysql::{MySql, MySqlConnection},
    Connection, Error, Executor, Row,
};
use sqlx_test::new;

fn mysql_supports_check_constraints(version: &str) -> bool {
    if version.contains("MariaDB") {
        return true;
    }

    let numeric = match version.split(|c| c == '-' || c == ' ').next() {
        Some(numeric) => numeric,
        None => return false,
    };
    let mut parts = numeric.split('.');
    let major: u64 = match parts.next().and_then(|part| part.parse().ok()) {
        Some(major) => major,
        None => return false,
    };
    let minor: u64 = parts.next().unwrap_or("0").parse().unwrap_or_default();
    let patch: u64 = parts.next().unwrap_or("0").parse().unwrap_or_default();

    (major, minor, patch) >= (8, 0, 16)
}

#[sqlx_macros::test]
async fn it_fails_with_unique_violation() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let mut tx = conn.begin().await?;

    sqlx::query("INSERT INTO tweet(id, text, owner_id) VALUES (1, 'Foo', 1)")
        .execute(&mut *tx)
        .await?;

    let res: Result<_, sqlx::Error> = sqlx::query("INSERT INTO tweet VALUES (1, NOW(), 'Foo', 1);")
        .execute(&mut *tx)
        .await;
    let err = res.unwrap_err();

    let err = err.into_database_error().unwrap();

    assert_eq!(err.kind(), ErrorKind::UniqueViolation);

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_foreign_key_violation() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let mut tx = conn.begin().await?;

    let res: Result<_, sqlx::Error> =
        sqlx::query("INSERT INTO tweet_reply (tweet_id, text) VALUES (1, 'Reply!');")
            .execute(&mut *tx)
            .await;
    let err = res.unwrap_err();

    let err = err.into_database_error().unwrap();

    assert_eq!(err.kind(), ErrorKind::ForeignKeyViolation);

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_not_null_violation() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let mut tx = conn.begin().await?;

    let res: Result<_, sqlx::Error> = sqlx::query("INSERT INTO tweet (text) VALUES (null);")
        .execute(&mut *tx)
        .await;
    let err = res.unwrap_err();

    let err = err.into_database_error().unwrap();

    assert_eq!(err.kind(), ErrorKind::NotNullViolation);

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_check_violation() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let mut tx = conn.begin().await?;

    let version: String = sqlx::query_scalar("SELECT VERSION()")
        .fetch_one(&mut *tx)
        .await?;
    if !mysql_supports_check_constraints(&version) {
        return Ok(());
    }

    let res: Result<_, sqlx::Error> =
        sqlx::query("INSERT INTO products VALUES (1, 'Product 1', 0);")
            .execute(&mut *tx)
            .await;
    let err = res.unwrap_err();

    let err = err.into_database_error().unwrap();

    assert_eq!(err.kind(), ErrorKind::CheckViolation);

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_begin_failed() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let res = conn.begin_with("SELECT * FROM tweet").await;

    let err = res.unwrap_err();

    assert!(matches!(err, Error::BeginFailed), "{err:?}");

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_invalid_save_point_statement() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let mut txn = conn.begin().await?;
    let txn_conn = sqlx::Acquire::acquire(&mut txn).await?;
    let res = txn_conn.begin_with("BEGIN").await;

    let err = res.unwrap_err();

    assert!(matches!(err, Error::InvalidSavePointStatement), "{err}");

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_parameter_count_mismatch_too_few() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let res: Result<_, sqlx::Error> =
        sqlx::query("SELECT * FROM tweet WHERE id = ? AND owner_id = ?")
            .bind(1_i64)
            .execute(&mut conn)
            .await;

    let err = res.unwrap_err();

    assert!(matches!(err, Error::Protocol(_)), "{err}");

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_parameter_count_mismatch_too_many() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let res: Result<_, sqlx::Error> = sqlx::query("SELECT * FROM tweet WHERE id = ?")
        .bind(1_i64)
        .bind(2_i64)
        .execute(&mut conn)
        .await;

    let err = res.unwrap_err();

    assert!(matches!(err, Error::Protocol(_)), "{err}");

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_parameter_count_mismatch_zero_expected() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let res: Result<_, sqlx::Error> = sqlx::query("SELECT COUNT(*) FROM tweet")
        .bind(1_i64)
        .execute(&mut conn)
        .await;

    let err = res.unwrap_err();

    assert!(matches!(err, Error::Protocol(_)), "{err}");

    Ok(())
}

#[sqlx_macros::test]
async fn it_stays_usable_after_parameter_count_mismatch() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;

    let (prepared_before, closed_before) = stmt_prepare_close_counts(&mut conn).await?;

    for persistent in [true, false] {
        let res = sqlx::query("SELECT * FROM tweet WHERE id = ?")
            .persistent(persistent)
            .execute(&mut conn)
            .await;

        assert!(matches!(res, Err(Error::Protocol(_))), "{res:?}");

        // The check fails before COM_STMT_EXECUTE, so no response is pending.
        sqlx_core::rt::timeout(std::time::Duration::from_secs(5), conn.ping())
            .await
            .with_context(|| format!("ping hangs, persistent({persistent})"))?
            .with_context(|| format!("ping fails, persistent({persistent})"))?;
    }

    // The persistent statement stays in the cache, and the other one is
    // closed.
    let (prepared_after, closed_after) = stmt_prepare_close_counts(&mut conn).await?;
    let prepared = prepared_after - prepared_before;
    let closed = closed_after - closed_before;
    let cached = conn.cached_statements_size();
    assert_eq!(
        (prepared, closed, cached),
        (2, 1, 1),
        "(prepared, closed, cached)"
    );

    Ok(())
}

// The session's Com_stmt_prepare and Com_stmt_close counts.
async fn stmt_prepare_close_counts(conn: &mut MySqlConnection) -> anyhow::Result<(u64, u64)> {
    let rows = conn
        .fetch_all(
            "SHOW SESSION STATUS WHERE Variable_name IN ('Com_stmt_prepare', 'Com_stmt_close')",
        )
        .await?;

    let counter = |name: &str| -> anyhow::Result<u64> {
        let row = rows
            .iter()
            .find(|row| row.get::<&str, _>(0).eq_ignore_ascii_case(name))
            .with_context(|| format!("{name} is missing from SHOW SESSION STATUS"))?;

        Ok(row.try_get::<&str, _>(1)?.parse()?)
    };

    Ok((counter("Com_stmt_prepare")?, counter("Com_stmt_close")?))
}
