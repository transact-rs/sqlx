use sqlx::PgPool;

// `sqlx.toml` in this crate sets `common.database-url-var = "ACCOUNTS_DATABASE_URL"`,
// so `#[sqlx::test]` must create the test database through that variable.
// CI runs this test without `DATABASE_URL` set.
#[sqlx::test]
async fn it_uses_configured_database_url_var(pool: PgPool) -> sqlx::Result<()> {
    let db_name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await?;

    assert!(db_name.starts_with("_sqlx_test"), "dbname: {db_name:?}");

    // The migrations of this crate must be applied.
    let account_exists: bool = sqlx::query_scalar("SELECT to_regclass('account') IS NOT NULL")
        .fetch_one(&pool)
        .await?;

    assert!(account_exists);

    Ok(())
}
