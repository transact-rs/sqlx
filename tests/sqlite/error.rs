use sqlx::{error::ErrorKind, sqlite::Sqlite, sqlite::SqliteError, Connection, Error};
use sqlx_test::new;

#[sqlx_macros::test]
async fn it_fails_with_unique_violation() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let mut tx = conn.begin().await?;

    let res: Result<_, sqlx::Error> = sqlx::query("INSERT INTO tweet VALUES (1, 'Foo', true, 1);")
        .execute(&mut *tx)
        .await;
    let err = res.unwrap_err();

    let err = err.into_database_error().unwrap();

    assert_eq!(err.kind(), ErrorKind::UniqueViolation);
    assert_eq!(err.downcast_ref::<SqliteError>().byte_offset(), None);

    Ok(())
}

#[cfg(feature = "sqlite-error-offset")]
#[sqlx_macros::test]
async fn it_reports_the_byte_offset_in_the_submitted_sql() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    for (sql, token) in [
        ("SELEC 1", Some("SELEC")),
        ("\u{2003}  SELECT 'é', FROM t", Some("FROM")),
        ("\tSELECT 'é'; ; -- skip this\n SELEC 2  ", Some("SELEC ")),
        ("SELECT * FROM missing_table", None),
    ] {
        let err = sqlx::raw_sql(sql).execute(&mut conn).await.unwrap_err();
        let err = err.into_database_error().unwrap();
        let offset = err.downcast_ref::<SqliteError>().byte_offset();
        assert_eq!(offset, token.map(|t| sql.find(t).unwrap()), "{sql}");
    }

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_foreign_key_violation() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let mut tx = conn.begin().await?;

    let res: Result<_, sqlx::Error> =
        sqlx::query("INSERT INTO tweet_reply (id, tweet_id, text) VALUES (2, 2, 'Reply!');")
            .execute(&mut *tx)
            .await;
    let err = res.unwrap_err();

    let err = err.into_database_error().unwrap();

    assert_eq!(err.kind(), ErrorKind::ForeignKeyViolation);

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_not_null_violation() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
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
    let mut conn = new::<Sqlite>().await?;
    let mut tx = conn.begin().await?;

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
    let mut conn = new::<Sqlite>().await?;
    let res = conn.begin_with("SELECT * FROM tweet").await;

    let err = res.unwrap_err();

    assert!(matches!(err, Error::BeginFailed), "{err:?}");

    Ok(())
}

#[sqlx_macros::test]
async fn it_fails_with_invalid_save_point_statement() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let mut txn = conn.begin().await?;
    let txn_conn = sqlx::Acquire::acquire(&mut txn).await?;
    let res = txn_conn.begin_with("BEGIN").await;

    let err = res.unwrap_err();

    assert!(matches!(err, Error::InvalidSavePointStatement), "{err}");

    Ok(())
}
