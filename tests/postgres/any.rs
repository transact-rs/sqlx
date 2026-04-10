use sqlx::Any;
use sqlx_test::new;

/// ensure Any type with PostgreSQL backing returns last_insert_id properly
/// https://github.com/launchbadge/sqlx/issues/2982
#[sqlx_macros::test]
async fn any_sets_last_insert_id() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let mut conn = new::<Any>().await?;
    // syntax as per: https://www.postgresql.org/docs/current/ddl-identity-columns.html
    let _ = sqlx::query(
        "CREATE TEMPORARY TABLE users (id INTEGER GENERATED ALWAYS AS IDENTITY, name TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await?;

    let result = sqlx::query("INSERT INTO users (name) VALUES (?)")
        .bind("Glorbo")
        .execute(&mut conn)
        .await?;

    // NOTE: PgQueryResult does not implement an equivalent concept and can only return None
    assert_eq!(result.last_insert_id(), None);

    Ok(())
}
