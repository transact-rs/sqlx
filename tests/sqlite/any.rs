use sqlx::Any;
use sqlx_test::new;

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

/// ensure Any type with SQLite backing returns last_insert_id properly
/// https://github.com/launchbadge/sqlx/issues/2982
#[sqlx_macros::test]
async fn any_sets_last_insert_id() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let mut conn = new::<Any>().await?;
    // syntax as per: https://sqlite.org/autoinc.html
    let _ = sqlx::query(
        "CREATE TEMPORARY TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await?;

    let result = sqlx::query("INSERT INTO users (name) VALUES (?)")
        .bind("Glorbo")
        .execute(&mut conn)
        .await?;

    assert_eq!(result.last_insert_id(), Some(1));

    Ok(())
}
