use sqlx::sqlite::SqliteOwnedBuf;
use sqlx::{Connection, SqliteConnection};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut conn = SqliteConnection::connect("sqlite::memory:").await?;

    sqlx::raw_sql(
        "create table notes(id integer primary key, body text not null);
         insert into notes(body) values ('hello'), ('world');",
    )
    .execute(&mut conn)
    .await?;

    let snapshot: SqliteOwnedBuf = conn.serialize(None).await?;
    let bytes: &[u8] = snapshot.as_ref();
    conn.close().await?;

    let owned = SqliteOwnedBuf::try_from(bytes)?;
    let mut restored = SqliteConnection::connect("sqlite::memory:").await?;
    restored.deserialize(None, owned, false).await?;

    let rows = sqlx::query_as::<_, (i64, String)>("select id, body from notes order by id")
        .fetch_all(&mut restored)
        .await?;

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "hello");
    assert_eq!(rows[1].1, "world");
    Ok(())
}
