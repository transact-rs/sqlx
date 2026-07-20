use sqlx::any::{install_default_drivers, AnyRow};
use sqlx::{Any, Connection, Executor, Row};
use sqlx_core::error::BoxDynError;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_core::Error;
use sqlx_test::new;

#[cfg(feature = "json")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "json")]
use sqlx::types::Json;

#[sqlx_macros::test]
async fn it_connects() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let mut conn = new::<Any>().await?;

    let value = sqlx::query("select 1 + 5")
        .try_map(|row: AnyRow| row.try_get::<i32, _>(0))
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(6i32, value);

    conn.close().await?;

    Ok(())
}

#[sqlx_macros::test]
async fn it_pings() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let mut conn = new::<Any>().await?;

    conn.ping().await?;

    Ok(())
}

#[sqlx_macros::test]
async fn it_executes_with_pool() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let pool = sqlx_test::pool::<Any>().await?;

    let rows = pool.fetch_all("SELECT 1; SElECT 2").await?;

    assert_eq!(rows.len(), 2);

    Ok(())
}

#[sqlx_macros::test]
async fn it_does_not_stop_stream_after_decoding_error() -> anyhow::Result<()> {
    use futures_util::stream::StreamExt;

    sqlx::any::install_default_drivers();

    // see https://github.com/launchbadge/sqlx/issues/1884
    let pool = sqlx_test::pool::<Any>().await?;

    #[derive(Debug, PartialEq)]
    struct MyType;
    impl<'a> sqlx::FromRow<'a, AnyRow> for MyType {
        fn from_row(row: &'a AnyRow) -> sqlx::Result<Self> {
            let n = row.try_get::<i32, _>(0)?;
            if n == 1 {
                Err(sqlx::Error::RowNotFound)
            } else {
                Ok(MyType)
            }
        }
    }

    let rows = sqlx::query_as("SELECT 0 UNION ALL SELECT 1 UNION ALL SELECT 2")
        .fetch(&pool)
        .map(|r| r.ok())
        .collect::<Vec<_>>()
        .await;

    assert_eq!(rows, vec![Some(MyType), None, Some(MyType)]);
    Ok(())
}

#[sqlx_macros::test]
async fn it_gets_by_name() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let mut conn = new::<Any>().await?;

    let row = conn.fetch_one("SELECT 1 as _1").await?;
    let val: i32 = row.get("_1");

    assert_eq!(val, 1);

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_fail_and_recover() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let mut conn = new::<Any>().await?;

    for i in 0..10 {
        // make a query that will fail
        let res = conn
            .execute("INSERT INTO not_found (column) VALUES (10)")
            .await;

        assert!(res.is_err());

        // now try and use the connection
        let val: i32 = conn
            .fetch_one(AssertSqlSafe(format!("SELECT {i}")))
            .await?
            .get_unchecked(0);

        assert_eq!(val, i);
    }

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_fail_and_recover_with_pool() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    let pool = sqlx_test::pool::<Any>().await?;

    for i in 0..10 {
        // make a query that will fail
        let res = pool
            .execute("INSERT INTO not_found (column) VALUES (10)")
            .await;

        assert!(res.is_err());

        // now try and use the connection
        let val: i32 = pool
            .fetch_one(AssertSqlSafe(format!("SELECT {i}")))
            .await?
            .get_unchecked(0);

        assert_eq!(val, i);
    }

    Ok(())
}

#[sqlx_macros::test]
async fn it_can_query_by_string_args() -> sqlx::Result<()> {
    install_default_drivers();

    let mut conn = new::<Any>().await?;

    let string = "Hello, world!".to_string();
    let ref tuple = ("Hello, world!".to_string(),);

    #[cfg(feature = "postgres")]
    const SQL: &str = "SELECT 'Hello, world!' \
        FROM (SELECT 1) AS t \
        WHERE 'Hello, world!' IN ($1, $2, $3, $4, $5, $6, $7)";

    #[cfg(not(feature = "postgres"))]
    const SQL: &str = "SELECT 'Hello, world!' \
        FROM (SELECT 1) AS t \
        WHERE 'Hello, world!' IN (?, ?, ?, ?, ?, ?, ?)";

    {
        let query = sqlx::query(SQL)
            // validate flexibility of lifetimes
            .bind(&string)
            .bind(&string[..])
            .bind(Some(&string))
            .bind(Some(&string[..]))
            .bind(&Option::<String>::None)
            .bind(&string.clone())
            .bind(&tuple.0); // should not get "temporary value is freed at the end of this statement" here

        let result = query.fetch_one(&mut conn).await?;

        let column_0: String = result.try_get(0)?;

        assert_eq!(column_0, string);
    }

    {
        let mut query = sqlx::query(SQL);

        let query = || -> Result<_, BoxDynError> {
            // validate flexibility of lifetimes
            query.try_bind(&string)?;
            query.try_bind(&string[..])?;
            query.try_bind(Some(&string))?;
            query.try_bind(Some(&string[..]))?;
            query.try_bind(&Option::<String>::None)?;
            query.try_bind(&string.clone())?;
            query.try_bind(&tuple.0)?;

            Ok(query)
        }()
        .map_err(Error::Encode)?;

        let result = query.fetch_one(&mut conn).await?;

        let column_0: String = result.try_get(0)?;

        assert_eq!(column_0, string);
    }
    Ok(())
}

#[cfg(feature = "json")]
#[sqlx_macros::test]
async fn it_encodes_decodes_json() -> anyhow::Result<()> {
    sqlx::any::install_default_drivers();

    // Create new connection
    let mut conn = new::<Any>().await?;

    // Test with serde_json::Value
    let json_value = serde_json::json!({
        "name": "test",
        "value": 42,
        "items": [1, 2, 3]
    });

    // Create temp table:
    sqlx::query("create temporary table json_test (data TEXT)")
        .execute(&mut conn)
        .await?;

    #[cfg(feature = "postgres")]
    let query = "insert into json_test (data) values ($1)";

    #[cfg(not(feature = "postgres"))]
    let query = "insert into json_test (data) values (?)";

    // Insert into the temporary table:
    sqlx::query(query)
        .bind(Json(&json_value))
        .execute(&mut conn)
        .await?;

    // This will work by encoding JSON as text and decoding it back
    let result: serde_json::Value = sqlx::query_scalar("select data from json_test")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(result, json_value);

    // Test with custom struct
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct TestData {
        name: String,
        value: i32,
        items: [i32; 3],
    }

    let test_data = TestData {
        name: "test".to_string(),
        value: 42,
        items: [1, 2, 3],
    };

    let result: Json<TestData> = sqlx::query_scalar("select data from json_test")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(result.0, test_data);

    Ok(())
}
