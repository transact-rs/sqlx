use sqlx::Sqlite;
use sqlx_test::{new, test_type};

#[derive(Debug, PartialEq, sqlx::Type)]
#[repr(u32)]
enum Origin {
    Foo = 1,
    Bar = 2,
}

test_type!(origin_enum<Origin>(Sqlite,
    "1" == Origin::Foo,
    "2" == Origin::Bar,
));

#[derive(PartialEq, Eq, Debug, sqlx::Type)]
#[sqlx(transparent)]
struct TransparentTuple(i64);

#[derive(PartialEq, Eq, Debug, sqlx::Type)]
#[sqlx(transparent)]
struct TransparentNamed {
    field: i64,
}

test_type!(transparent_tuple<TransparentTuple>(Sqlite,
    "0" == TransparentTuple(0),
    "23523" == TransparentTuple(23523)
));

test_type!(transparent_named<TransparentNamed>(Sqlite,
    "0" == TransparentNamed { field: 0 },
    "23523" == TransparentNamed { field: 23523 },
));

// SQLite stores JSON as TEXT. The `#[sqlx(json)]` field attribute lets a struct
// read such a column into a `serde_json::Value` (or any `Deserialize` type),
// deserializing it with `serde_json` during decode.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct JsonRow {
    id: i64,
    #[sqlx(json)]
    data: serde_json::Value,
}

#[sqlx_macros::test]
async fn it_reads_json_from_text_column() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let row: JsonRow = sqlx::query_as(
        r#"SELECT 1 AS id, '{"key": "value"}' AS data"#,
    )
    .fetch_one(&mut conn)
    .await?;

    assert_eq!(
        row,
        JsonRow {
            id: 1,
            data: serde_json::json!({ "key": "value" }),
        }
    );

    Ok(())
}

#[sqlx_macros::test]
async fn it_surfaces_invalid_json_as_column_decode_error() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let result: Result<JsonRow, sqlx::Error> =
        sqlx::query_as(r#"SELECT 1 AS id, 'not valid json' AS data"#)
            .fetch_one(&mut conn)
            .await;

    match result {
        Err(sqlx::Error::ColumnDecode { index, .. }) => {
            assert_eq!(index, "\"data\"");
        }
        other => panic!("expected ColumnDecode error, got {other:?}"),
    }

    Ok(())
}
