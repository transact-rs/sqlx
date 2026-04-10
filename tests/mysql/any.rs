use sqlx::Any;
use sqlx_test::new;

/// ensure Any type with MySQL backing returns last_insert_id properly
/// https://github.com/launchbadge/sqlx/issues/2982
#[sqlx_macros::test]
async fn any_sets_last_insert_id() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let mut conn = new::<Any>().await?;
    // syntax as per: https://dev.mysql.com/doc/refman/9.6/en/example-auto-increment.html
    let _ = sqlx::query(
        "CREATE TEMPORARY TABLE users (id INTEGER NOT NULL PRIMARY KEY AUTO_INCREMENT, name TEXT NOT NULL)",
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
