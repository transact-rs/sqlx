use sqlx::Any;
use sqlx_test::new;

// Regression test for https://github.com/launchbadge/sqlx/issues/2982
// `map_result()` in sqlx-sqlite/src/any.rs was discarding `last_insert_rowid`,
// always returning `last_insert_id: None` in `AnyQueryResult`.
#[sqlx_macros::test]
async fn any_query_result_has_last_insert_id() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();
    let mut conn = new::<Any>().await?;

    sqlx::query(
        "CREATE TEMPORARY TABLE any_last_id_test (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await?;

    let result = sqlx::query("INSERT INTO any_last_id_test (name) VALUES (?)")
        .bind("Alice")
        .execute(&mut conn)
        .await?;

    assert_eq!(
        result.last_insert_id(),
        Some(1),
        "first insert should return id 1"
    );

    let result = sqlx::query("INSERT INTO any_last_id_test (name) VALUES (?)")
        .bind("Bob")
        .execute(&mut conn)
        .await?;

    assert_eq!(
        result.last_insert_id(),
        Some(2),
        "second insert should return id 2"
    );

    Ok(())
}

#[sqlx_macros::test]
async fn it_encodes_bool_with_any() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();
    let mut conn = new::<Any>().await?;

    let res = sqlx::query("INSERT INTO accounts (name, is_active) VALUES (?, ?)")
        .bind("Harrison Ford")
        .bind(true)
        .execute(&mut conn)
        .await
        .expect("failed to encode bool");
    assert_eq!(res.rows_affected(), 1);

    Ok(())
}

#[sqlx_macros::test]
async fn issue_3179() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let mut conn = new::<Any>().await?;

    // 4294967297 = 2^32
    let number: i64 = sqlx::query_scalar("SELECT 4294967296")
        .fetch_one(&mut conn)
        .await?;

    // Previously, the decoding would use `i32` as an intermediate which would overflow to 0.
    assert_eq!(number, 4294967296);

    Ok(())
}
