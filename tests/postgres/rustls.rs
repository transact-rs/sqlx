use sqlx_core::connection::ConnectOptions;
use sqlx_postgres::PgConnectOptions;
use sqlx_test::setup_if_needed;

#[sqlx_macros::test]
async fn connects_with_rustls_graviola() -> anyhow::Result<()> {
    setup_if_needed();

    let opts = dotenvy::var("DATABASE_URL")?
        .parse::<PgConnectOptions>()?
        .rustls_crypto_provider(rustls_graviola::default_provider());

    let mut conn = opts.connect().await?;

    let res: i64 = sqlx::query_scalar("SELECT 1").fetch_one(&mut conn).await?;

    assert_eq!(res, 1);

    Ok(())
}
