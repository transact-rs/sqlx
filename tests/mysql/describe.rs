use sqlx::mysql::MySql;
use sqlx::{Column, Executor, SqlSafeStr, Type, TypeInfo};
use sqlx_test::new;

#[sqlx_macros::test]
async fn it_describes_simple() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;

    let d = conn.describe("SELECT * FROM tweet".into_sql_str()).await?;

    assert_eq!(d.columns()[0].name(), "id");
    assert_eq!(d.columns()[1].name(), "created_at");
    assert_eq!(d.columns()[2].name(), "text");
    assert_eq!(d.columns()[3].name(), "owner_id");

    assert_eq!(d.nullable(0), Some(false));
    // `created_at` is `TIMESTAMP NOT NULL`, but a "zero date" (`0000-00-00 00:00:00`)
    // is decoded as `NULL`/`None` (see `MySqlValueRef::is_null`), so SQLx reports
    // temporal columns as nullable regardless of the `NOT_NULL` flag.
    assert_eq!(d.nullable(1), Some(true));
    assert_eq!(d.nullable(2), Some(false));
    assert_eq!(d.nullable(3), Some(true));

    assert_eq!(d.columns()[0].type_info().name(), "BIGINT");
    assert_eq!(d.columns()[1].type_info().name(), "TIMESTAMP");
    assert_eq!(d.columns()[2].type_info().name(), "TEXT");
    assert_eq!(d.columns()[3].type_info().name(), "BIGINT");

    Ok(())
}

// `DATE`/`DATETIME`/`TIMESTAMP` columns can yield a "zero date" which SQLx decodes
// as `NULL`/`None`, so they must be inferred as nullable even when declared `NOT NULL`
// (https://github.com/launchbadge/sqlx/issues/4283). `TIME` has no zero value that maps
// to NULL, so it (and non-temporal types) must keep the `NOT NULL` inference.
#[sqlx_macros::test]
async fn it_describes_temporal_not_null_as_nullable() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;

    conn.execute(
        r#"
CREATE TEMPORARY TABLE temporal_not_null (
    d        DATE      NOT NULL,
    dt       DATETIME  NOT NULL,
    ts       TIMESTAMP NOT NULL,
    t        TIME      NOT NULL,
    n        INTEGER   NOT NULL,
    nullable INTEGER
);
    "#,
    )
    .await?;

    let d = conn
        .describe("SELECT * FROM temporal_not_null".into_sql_str())
        .await?;

    // zero-date-bearing temporal types: nullable despite `NOT NULL`
    assert_eq!(d.column(0).name(), "d");
    assert_eq!(d.nullable(0), Some(true));
    assert_eq!(d.column(1).name(), "dt");
    assert_eq!(d.nullable(1), Some(true));
    assert_eq!(d.column(2).name(), "ts");
    assert_eq!(d.nullable(2), Some(true));

    // `TIME` and non-temporal `NOT NULL` columns stay non-nullable
    assert_eq!(d.column(3).name(), "t");
    assert_eq!(d.nullable(3), Some(false));
    assert_eq!(d.column(4).name(), "n");
    assert_eq!(d.nullable(4), Some(false));

    // an actually-nullable column is still nullable
    assert_eq!(d.column(5).name(), "nullable");
    assert_eq!(d.nullable(5), Some(true));

    Ok(())
}

#[sqlx_macros::test]
async fn test_boolean() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;

    conn.execute(
        r#"
CREATE TEMPORARY TABLE with_bit_and_tinyint (
    id INT PRIMARY KEY AUTO_INCREMENT,
    value_bit_1 BIT(1),
    value_bool BOOLEAN,
    bit_n BIT(64),
    value_int TINYINT
);
    "#,
    )
    .await?;

    let d = conn
        .describe("SELECT * FROM with_bit_and_tinyint".into_sql_str())
        .await?;

    assert_eq!(d.column(2).name(), "value_bool");
    assert_eq!(d.column(2).type_info().name(), "BOOLEAN");

    assert_eq!(d.column(1).name(), "value_bit_1");
    assert_eq!(d.column(1).type_info().name(), "BIT");

    assert!(<bool as Type<MySql>>::compatible(&d.column(1).type_info()));
    assert!(<bool as Type<MySql>>::compatible(&d.column(2).type_info()));

    Ok(())
}

#[sqlx_macros::test]
async fn uses_alias_name() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;

    let d = conn
        .describe("SELECT text AS tweet_text FROM tweet".into_sql_str())
        .await?;

    assert_eq!(d.columns()[0].name(), "tweet_text");

    Ok(())
}
