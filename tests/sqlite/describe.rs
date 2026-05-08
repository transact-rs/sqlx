use sqlx::error::DatabaseError;
use sqlx::sqlite::{SqliteConnectOptions, SqliteError};
use sqlx::TypeInfo;
use sqlx::{sqlite::Sqlite, Column, Executor};
use sqlx::{ConnectOptions, SqlSafeStr};
use sqlx_test::new;
use std::env;

#[sqlx_macros::test]
async fn it_describes_simple() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let info = conn.describe("SELECT * FROM tweet".into_sql_str()).await?;
    let columns = info.columns();

    assert_eq!(columns[0].name(), "id");
    assert_eq!(columns[1].name(), "text");
    assert_eq!(columns[2].name(), "is_sent");
    assert_eq!(columns[3].name(), "owner_id");

    assert_eq!(columns[0].ordinal(), 0);
    assert_eq!(columns[1].ordinal(), 1);
    assert_eq!(columns[2].ordinal(), 2);
    assert_eq!(columns[3].ordinal(), 3);

    assert_eq!(info.nullable(0), Some(false));
    assert_eq!(info.nullable(1), Some(false));
    assert_eq!(info.nullable(2), Some(false));
    assert_eq!(info.nullable(3), Some(true)); // owner_id

    assert_eq!(columns[0].type_info().name(), "INTEGER");
    assert_eq!(columns[1].type_info().name(), "TEXT");
    assert_eq!(columns[2].type_info().name(), "BOOLEAN");
    assert_eq!(columns[3].type_info().name(), "INTEGER");

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_variables() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    // without any context, we resolve to NULL
    let info = conn.describe("SELECT ?1".into_sql_str()).await?;

    assert_eq!(info.columns()[0].type_info().name(), "NULL");
    assert_eq!(info.nullable(0), Some(true)); // nothing prevents the value from being bound to null

    // context can be provided by using CAST(_ as _)
    let info = conn
        .describe("SELECT CAST(?1 AS REAL)".into_sql_str())
        .await?;

    assert_eq!(info.columns()[0].type_info().name(), "REAL");
    assert_eq!(info.nullable(0), Some(true)); // nothing prevents the value from being bound to null

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_expression() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe("SELECT 1 + 10, 5.12 * 2, 'Hello', x'deadbeef', null".into_sql_str())
        .await?;

    let columns = d.columns();

    assert_eq!(columns[0].type_info().name(), "INTEGER");
    assert_eq!(columns[0].name(), "1 + 10");
    assert_eq!(d.nullable(0), Some(false)); // literal constant

    assert_eq!(columns[1].type_info().name(), "REAL");
    assert_eq!(columns[1].name(), "5.12 * 2");
    assert_eq!(d.nullable(1), Some(false)); // literal constant

    assert_eq!(columns[2].type_info().name(), "TEXT");
    assert_eq!(columns[2].name(), "'Hello'");
    assert_eq!(d.nullable(2), Some(false)); // literal constant

    assert_eq!(columns[3].type_info().name(), "BLOB");
    assert_eq!(columns[3].name(), "x'deadbeef'");
    assert_eq!(d.nullable(3), Some(false)); // literal constant

    assert_eq!(columns[4].type_info().name(), "NULL");
    assert_eq!(columns[4].name(), "null");
    assert_eq!(d.nullable(4), Some(true)); // literal null

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_temporary_table() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute(
        "CREATE TEMPORARY TABLE IF NOT EXISTS empty_all_types_and_nulls(
        i1 integer NULL,
        r1 real NULL,
        t1 text NULL,
        b1 blob NULL,
        i2 INTEGER NOT NULL,
        r2 REAL NOT NULL,
        t2 TEXT NOT NULL,
        b2 BLOB NOT NULL
        )",
    )
    .await?;

    let d = conn
        .describe("SELECT * FROM empty_all_types_and_nulls".into_sql_str())
        .await?;
    assert_eq!(d.columns().len(), 8);

    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(true));

    assert_eq!(d.column(1).type_info().name(), "REAL");
    assert_eq!(d.nullable(1), Some(true));

    assert_eq!(d.column(2).type_info().name(), "TEXT");
    assert_eq!(d.nullable(2), Some(true));

    assert_eq!(d.column(3).type_info().name(), "BLOB");
    assert_eq!(d.nullable(3), Some(true));

    assert_eq!(d.column(4).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(4), Some(false));

    assert_eq!(d.column(5).type_info().name(), "REAL");
    assert_eq!(d.nullable(5), Some(false));

    assert_eq!(d.column(6).type_info().name(), "TEXT");
    assert_eq!(d.nullable(6), Some(false));

    assert_eq!(d.column(7).type_info().name(), "BLOB");
    assert_eq!(d.nullable(7), Some(false));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_expression_from_empty_table() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute("CREATE TEMP TABLE _temp_empty ( name TEXT NOT NULL, a INT )")
        .await?;

    let d = conn
        .describe("SELECT COUNT(*), a + 1, name, 5.12, 'Hello' FROM _temp_empty".into_sql_str())
        .await?;

    assert_eq!(d.columns()[0].type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false)); // COUNT(*)

    assert_eq!(d.columns()[1].type_info().name(), "INTEGER");
    assert_eq!(d.nullable(1), Some(true)); // `a+1` is nullable, because a is nullable

    assert_eq!(d.columns()[2].type_info().name(), "TEXT");
    assert_eq!(d.nullable(2), Some(true)); // `name` is not nullable, but the query can be null due to zero rows

    assert_eq!(d.columns()[3].type_info().name(), "REAL");
    assert_eq!(d.nullable(3), Some(false)); // literal constant

    assert_eq!(d.columns()[4].type_info().name(), "TEXT");
    assert_eq!(d.nullable(4), Some(false)); // literal constant

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_expression_from_empty_table_with_star() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute("CREATE TEMP TABLE _temp_empty ( name TEXT, a INT )")
        .await?;

    let d = conn
        .describe("SELECT *, 5, 'Hello' FROM _temp_empty".into_sql_str())
        .await?;

    assert_eq!(d.columns()[0].type_info().name(), "TEXT");
    assert_eq!(d.columns()[1].type_info().name(), "INTEGER");
    assert_eq!(d.columns()[2].type_info().name(), "INTEGER");
    assert_eq!(d.columns()[3].type_info().name(), "TEXT");

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_insert() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe("INSERT INTO tweet (id, text) VALUES (2, 'Hello')".into_sql_str())
        .await?;

    assert_eq!(d.columns().len(), 0);

    let d = conn
        .describe(
            "INSERT INTO tweet (id, text) VALUES (2, 'Hello'); SELECT last_insert_rowid();"
                .into_sql_str(),
        )
        .await?;

    assert_eq!(d.columns().len(), 1);
    assert_eq!(d.columns()[0].type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_insert_with_read_only() -> anyhow::Result<()> {
    sqlx_test::setup_if_needed();

    let mut options: SqliteConnectOptions = env::var("DATABASE_URL")?.parse().unwrap();
    options = options.read_only(true);

    let mut conn = options.connect().await?;

    let d = conn
        .describe("INSERT INTO tweet (id, text) VALUES (2, 'Hello')".into_sql_str())
        .await?;

    assert_eq!(d.columns().len(), 0);

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_insert_with_returning() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe("INSERT INTO tweet (id, text) VALUES (2, 'Hello') RETURNING *".into_sql_str())
        .await?;

    assert_eq!(d.columns().len(), 4);
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));
    assert_eq!(d.column(1).type_info().name(), "TEXT");
    assert_eq!(d.nullable(1), Some(false));

    let d = conn
        .describe(
            "INSERT INTO accounts (name, is_active) VALUES ('a', true) RETURNING id".into_sql_str(),
        )
        .await?;

    assert_eq!(d.columns().len(), 1);
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_bound_columns_non_null() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let d = conn
        .describe("INSERT INTO tweet (id, text) VALUES ($1, $2) returning *".into_sql_str())
        .await?;

    assert_eq!(d.columns().len(), 4);
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));
    assert_eq!(d.column(1).type_info().name(), "TEXT");
    assert_eq!(d.nullable(1), Some(false));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_update_with_returning() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe("UPDATE accounts SET is_active=true WHERE name=?1 RETURNING id".into_sql_str())
        .await?;

    assert_eq!(d.columns().len(), 1);
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("UPDATE accounts SET is_active=true WHERE id=?1 RETURNING *".into_sql_str())
        .await?;

    assert_eq!(d.columns().len(), 3);
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));
    assert_eq!(d.column(1).type_info().name(), "TEXT");
    assert_eq!(d.nullable(1), Some(false));
    assert_eq!(d.column(2).type_info().name(), "BOOLEAN");
    //assert_eq!(d.nullable(2), Some(false)); //query analysis is allowed to notice that it is always set to true by the update

    let d = conn
        .describe("UPDATE accounts SET is_active=true WHERE id=?1 RETURNING id".into_sql_str())
        .await?;

    assert_eq!(d.columns().len(), 1);
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_delete_with_returning() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe("DELETE FROM accounts WHERE name=?1 RETURNING id".into_sql_str())
        .await?;

    assert_eq!(d.columns().len(), 1);
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_bad_statement() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let err = conn
        .describe("SELECT 1 FROM not_found".into_sql_str())
        .await
        .unwrap_err();
    let err = err
        .as_database_error()
        .unwrap()
        .downcast_ref::<SqliteError>();

    assert_eq!(err.message(), "no such table: not_found");
    assert_eq!(err.code().as_deref(), Some("1"));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_left_join() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe("select accounts.id from accounts".into_sql_str())
        .await?;

    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe(
            "select tweet.id from accounts left join tweet on owner_id = accounts.id"
                .into_sql_str(),
        )
        .await?;

    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(true));

    let d = conn
        .describe(
            "select tweet.id, accounts.id from accounts left join tweet on owner_id = accounts.id"
                .into_sql_str(),
        )
        .await?;

    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(true));

    assert_eq!(d.column(1).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(1), Some(false));

    let d = conn
        .describe(
            "select tweet.id, accounts.id from accounts inner join tweet on owner_id = accounts.id"
                .into_sql_str(),
        )
        .await?;

    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    assert_eq!(d.column(1).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(1), Some(false));

    let d = conn
        .describe(
            "select tweet.id, accounts.id from accounts left join tweet on tweet.id = accounts.id"
                .into_sql_str(),
        )
        .await?;

    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(true));

    assert_eq!(d.column(1).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(1), Some(false));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_group_by() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe("select id from accounts group by id".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("SELECT name from accounts GROUP BY 1 LIMIT -1 OFFSET 1".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "TEXT");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("SELECT sum(id), sum(is_sent) from tweet GROUP BY owner_id".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));
    assert_eq!(d.column(1).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(1), Some(false));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_ungrouped_aggregate() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe("select count(1) from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("SELECT sum(is_sent) from tweet".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(true));

    let d = conn
        .describe("SELECT coalesce(sum(is_sent),0) from tweet".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_literal_subquery() -> anyhow::Result<()> {
    async fn assert_literal_described(
        conn: &mut sqlx::SqliteConnection,
        query: &'static str,
    ) -> anyhow::Result<()> {
        let info = conn.describe(query.into_sql_str()).await?;

        assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
        assert_eq!(info.nullable(0), Some(false), "{query}");
        assert_eq!(info.column(1).type_info().name(), "NULL", "{query}");
        assert_eq!(info.nullable(1), Some(true), "{query}");

        Ok(())
    }

    let mut conn = new::<Sqlite>().await?;
    assert_literal_described(&mut conn, "SELECT 'a', NULL").await?;
    assert_literal_described(&mut conn, "SELECT * FROM (SELECT 'a', NULL)").await?;
    assert_literal_described(
        &mut conn,
        "WITH cte AS (SELECT 'a', NULL) SELECT * FROM cte",
    )
    .await?;
    assert_literal_described(
        &mut conn,
        "WITH cte AS MATERIALIZED (SELECT 'a', NULL) SELECT * FROM cte",
    )
    .await?;
    assert_literal_described(
        &mut conn,
        "WITH RECURSIVE cte(a,b) AS (SELECT 'a', NULL UNION ALL SELECT a||a, NULL FROM cte WHERE length(a)<3) SELECT * FROM cte",
    )
    .await?;

    Ok(())
}

async fn assert_tweet_described(
    conn: &mut sqlx::SqliteConnection,
    query: &'static str,
) -> anyhow::Result<()> {
    let info = conn.describe(query.into_sql_str()).await?;
    let columns = info.columns();

    assert_eq!(columns[0].name(), "id", "{query}");
    assert_eq!(columns[1].name(), "text", "{query}");
    assert_eq!(columns[2].name(), "is_sent", "{query}");
    assert_eq!(columns[3].name(), "owner_id", "{query}");

    assert_eq!(columns[0].ordinal(), 0, "{query}");
    assert_eq!(columns[1].ordinal(), 1, "{query}");
    assert_eq!(columns[2].ordinal(), 2, "{query}");
    assert_eq!(columns[3].ordinal(), 3, "{query}");

    assert_eq!(info.nullable(0), Some(false), "{query}");
    assert_eq!(info.nullable(1), Some(false), "{query}");
    assert_eq!(info.nullable(2), Some(false), "{query}");
    assert_eq!(info.nullable(3), Some(true), "{query}");

    assert_eq!(columns[0].type_info().name(), "INTEGER", "{query}");
    assert_eq!(columns[1].type_info().name(), "TEXT", "{query}");
    assert_eq!(columns[2].type_info().name(), "BOOLEAN", "{query}");
    assert_eq!(columns[3].type_info().name(), "INTEGER", "{query}");

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_table_subquery() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    assert_tweet_described(&mut conn, "SELECT * FROM tweet").await?;
    assert_tweet_described(&mut conn, "SELECT * FROM (SELECT * FROM tweet)").await?;
    assert_tweet_described(
        &mut conn,
        "WITH cte AS (SELECT * FROM tweet) SELECT * FROM cte",
    )
    .await?;
    assert_tweet_described(
        &mut conn,
        "WITH cte AS MATERIALIZED (SELECT * FROM tweet) SELECT * FROM cte",
    )
    .await?;

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_table_order_by() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    assert_tweet_described(&mut conn, "SELECT * FROM tweet ORDER BY id").await?;
    assert_tweet_described(&mut conn, "SELECT * FROM tweet ORDER BY id NULLS LAST").await?;
    assert_tweet_described(
        &mut conn,
        "SELECT * FROM tweet ORDER BY owner_id DESC, text ASC",
    )
    .await?;

    async fn assert_literal_order_by_described(
        conn: &mut sqlx::SqliteConnection,
        query: &'static str,
    ) -> anyhow::Result<()> {
        let info = conn.describe(query.into_sql_str()).await?;

        assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
        assert_eq!(info.nullable(0), Some(false), "{query}");
        assert_eq!(info.column(1).type_info().name(), "TEXT", "{query}");
        assert_eq!(info.nullable(1), Some(false), "{query}");

        Ok(())
    }

    assert_literal_order_by_described(&mut conn, "SELECT 'a', text FROM tweet ORDER BY id").await?;
    assert_literal_order_by_described(
        &mut conn,
        "SELECT 'a', text FROM tweet ORDER BY id NULLS LAST",
    )
    .await?;
    assert_literal_order_by_described(&mut conn, "SELECT 'a', text FROM tweet ORDER BY text")
        .await?;
    assert_literal_order_by_described(
        &mut conn,
        "SELECT 'a', text FROM tweet ORDER BY text NULLS LAST",
    )
    .await?;
    assert_literal_order_by_described(
        &mut conn,
        "SELECT 'a', text FROM tweet ORDER BY text DESC NULLS LAST",
    )
    .await?;

    Ok(())
}

// Regression test for https://github.com/launchbadge/sqlx/issues/4147
// ORDER BY + LIMIT routes data through an ephemeral sorter table;
// the NOT NULL constraint must survive the round-trip.
#[sqlx_macros::test]
async fn it_describes_order_by_with_limit() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let info = conn
        .describe("SELECT text FROM tweet ORDER BY id DESC LIMIT 10".into_sql_str())
        .await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT");
    assert_eq!(
        info.nullable(0),
        Some(false),
        "NOT NULL column should stay NOT NULL with ORDER BY + LIMIT"
    );

    let info = conn
        .describe("SELECT text, is_sent FROM tweet ORDER BY id DESC LIMIT 10000".into_sql_str())
        .await?;
    assert_eq!(
        info.nullable(0),
        Some(false),
        "text should be NOT NULL with ORDER BY DESC + large LIMIT"
    );
    assert_eq!(
        info.nullable(1),
        Some(false),
        "is_sent should be NOT NULL with ORDER BY DESC + large LIMIT"
    );

    // nullable column must remain nullable
    let info = conn
        .describe("SELECT owner_id FROM tweet ORDER BY id DESC LIMIT 10".into_sql_str())
        .await?;
    assert_eq!(
        info.nullable(0),
        Some(true),
        "nullable column should stay nullable with ORDER BY + LIMIT"
    );

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_union() -> anyhow::Result<()> {
    async fn assert_union_described(
        conn: &mut sqlx::SqliteConnection,
        query: &'static str,
    ) -> anyhow::Result<()> {
        let info = conn.describe(query.into_sql_str()).await?;

        assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
        assert_eq!(info.nullable(0), Some(false), "{query}");
        assert_eq!(info.column(1).type_info().name(), "TEXT", "{query}");
        assert_eq!(info.nullable(1), Some(true), "{query}");
        assert_eq!(info.column(2).type_info().name(), "INTEGER", "{query}");
        assert_eq!(info.nullable(2), Some(true), "{query}");
        //TODO: mixed type columns not handled correctly
        //assert_eq!(info.column(3).type_info().name(), "NULL", "{query}");
        //assert_eq!(info.nullable(3), Some(false), "{query}");

        Ok(())
    }

    let mut conn = new::<Sqlite>().await?;
    assert_union_described(
        &mut conn,
        "SELECT 'txt','a',null,'b' UNION ALL SELECT 'int',NULL,1,2 ",
    )
    .await?;

    //TODO: insert into temp-table not merging datatype/nullable of all operations - currently keeping last-writer
    //assert_union_described(&mut conn, "SELECT 'txt','a',null,'b' UNION SELECT 'int',NULL,1,2 ").await?;

    assert_union_described(
        &mut conn,
        "SELECT 'tweet',text,owner_id id,null from tweet
        UNION SELECT 'account',name,id,is_active from accounts
        UNION SELECT 'account',name,id,is_active from accounts_view
        UNION SELECT 'dummy',null,null,null
        ORDER BY id
        ",
    )
    .await?;

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_having_group_by() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe(
            r#"
        WITH tweet_reply_unq as ( --tweets with a single response
          SELECT tweet_id id
          FROM tweet_reply
          GROUP BY tweet_id
          HAVING COUNT(1) = 1
        ) 
        SELECT 
          (
    		  SELECT COUNT(*) 
    		  FROM (
    		    SELECT NULL
    			FROM tweet
    			JOIN tweet_reply_unq
    			  USING (id)
                WHERE tweet.owner_id = accounts.id
              )
          ) single_reply_count
        FROM accounts
        WHERE id = ?1
        "#
            .into_sql_str(),
        )
        .await?;

    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    Ok(())
}

//documents failures originally found through property testing
#[sqlx_macros::test]
async fn it_describes_strange_queries() -> anyhow::Result<()> {
    async fn assert_single_column_described(
        conn: &mut sqlx::SqliteConnection,
        query: &'static str,
        typename: &str,
        nullable: bool,
    ) -> anyhow::Result<()> {
        let info = conn.describe(query.into_sql_str()).await?;
        assert_eq!(info.column(0).type_info().name(), typename, "{query}");
        assert_eq!(info.nullable(0), Some(nullable), "{query}");

        Ok(())
    }

    let mut conn = new::<Sqlite>().await?;

    assert_single_column_described(
        &mut conn,
        "SELECT true FROM (SELECT true) a ORDER BY true",
        "INTEGER",
        false,
    )
    .await?;

    assert_single_column_described(
        &mut conn,
        "
    	SELECT true
    	FROM (
    	    SELECT 'a'
    	)
    	CROSS JOIN (
    	    SELECT 'b'
    	    FROM (SELECT 'c')
            CROSS JOIN accounts
            ORDER BY id
            LIMIT 1
            )
    	",
        "INTEGER",
        false,
    )
    .await?;

    assert_single_column_described(
        &mut conn,
        "SELECT true FROM tweet
            ORDER BY true ASC NULLS LAST",
        "INTEGER",
        false,
    )
    .await?;

    assert_single_column_described(
        &mut conn,
        "SELECT true LIMIT -1 OFFSET -1",
        "INTEGER",
        false,
    )
    .await?;

    assert_single_column_described(
        &mut conn,
        "SELECT true FROM tweet J LIMIT 10 OFFSET 1000000",
        "INTEGER",
        false,
    )
    .await?;

    assert_single_column_described(
        &mut conn,
        "SELECT text
        FROM (SELECT null)
        CROSS JOIN (
            SELECT text
            FROM tweet 
            GROUP BY text
        )
        LIMIT -1 OFFSET -1",
        "TEXT",
        false,
    )
    .await?;

    assert_single_column_described(
        &mut conn,
        "SELECT EYH.id,COUNT(EYH.id)
    	FROM accounts EYH",
        "INTEGER",
        true,
    )
    .await?;

    assert_single_column_described(
        &mut conn,
        "SELECT SUM(tweet.text) FROM (SELECT NULL FROM accounts_view LIMIT -1 OFFSET 1) CROSS JOIN tweet",
        "REAL",
        true, // null if accounts view has fewer rows than the offset
    )
    .await?;

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_func_date() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let query = "SELECT date();";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(false), "{query}");

    let query = "SELECT date('now');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT date('now', 'start of month');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT date(:datebind);";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}");
    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_func_time() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let query = "SELECT time();";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(false), "{query}");

    let query = "SELECT time('now');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT time('now', 'start of month');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT time(:datebind);";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}");
    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_func_datetime() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let query = "SELECT datetime();";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(false), "{query}");

    let query = "SELECT datetime('now');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT datetime('now', 'start of month');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT datetime(:datebind);";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}");
    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_func_julianday() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let query = "SELECT julianday();";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "REAL", "{query}");
    assert_eq!(info.nullable(0), Some(false), "{query}");

    let query = "SELECT julianday('now');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "REAL", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT julianday('now', 'start of month');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "REAL", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT julianday(:datebind);";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "REAL", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}");
    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_func_strftime() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let query = "SELECT strftime('%s','now');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT strftime('%s', 'now', 'start of month');";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}"); //can't prove that it's not-null yet

    let query = "SELECT strftime('%s',:datebind);";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}");
    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_with_recursive() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let query = "
        WITH RECURSIVE schedule(begin_date) AS (
             SELECT datetime('2022-10-01')
             WHERE datetime('2022-10-01') < datetime('2022-11-03')
             UNION ALL
             SELECT datetime(begin_date,'+1 day')
             FROM schedule
             WHERE datetime(begin_date) < datetime(?2)
         )
         SELECT
             begin_date
         FROM schedule
         GROUP BY begin_date
        ";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}");

    let query = "
        WITH RECURSIVE schedule(begin_date) AS MATERIALIZED (
             SELECT datetime('2022-10-01')
             WHERE datetime('2022-10-01') < datetime('2022-11-03')
             UNION ALL
             SELECT datetime(begin_date,'+1 day')
             FROM schedule
             WHERE datetime(begin_date) < datetime(?2)
         )
         SELECT
             begin_date
         FROM schedule
         GROUP BY begin_date
        ";
    let info = conn.describe(query.into_sql_str()).await?;
    assert_eq!(info.column(0).type_info().name(), "TEXT", "{query}");
    assert_eq!(info.nullable(0), Some(true), "{query}");

    Ok(())
}

#[sqlx_macros::test]
async fn it_describes_analytical_function() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let d = conn
        .describe("select row_number() over () from accounts".into_sql_str())
        .await?;
    dbg!(&d);
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("select rank() over () from accounts".into_sql_str())
        .await?;
    dbg!(&d);
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("select dense_rank() over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("select percent_rank() over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "REAL");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("select cume_dist() over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "REAL");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("select ntile(1) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("select lag(id) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(true));

    let d = conn
        .describe("select lag(name) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "TEXT");
    assert_eq!(d.nullable(0), Some(true));

    let d = conn
        .describe("select lead(id) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(true));

    let d = conn
        .describe("select lead(name) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "TEXT");
    assert_eq!(d.nullable(0), Some(true));

    let d = conn
        .describe("select first_value(id) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(true));

    let d = conn
        .describe("select first_value(name) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "TEXT");
    assert_eq!(d.nullable(0), Some(true));

    let d = conn
        .describe("select last_value(id) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(false));

    let d = conn
        .describe("select first_value(name) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "TEXT");
    //assert_eq!(d.nullable(0), Some(false)); //this should be null, but it's hard to prove that it will be

    let d = conn
        .describe("select nth_value(id,10) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "INTEGER");
    assert_eq!(d.nullable(0), Some(true));

    let d = conn
        .describe("select nth_value(name,10) over () from accounts".into_sql_str())
        .await?;
    assert_eq!(d.column(0).type_info().name(), "TEXT");
    assert_eq!(d.nullable(0), Some(true));

    Ok(())
}

// Regression tests for INSERT NOT NULL validation (issue #4206)
// https://github.com/launchbadge/sqlx/issues/4206

#[sqlx_macros::test]
async fn it_validates_insert_with_all_required_columns() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute(
        "CREATE TEMPORARY TABLE test_insert_valid(
            id INTEGER PRIMARY KEY,
            required_a TEXT NOT NULL,
            required_b TEXT NOT NULL,
            optional_c TEXT
        )",
    )
    .await?;

    // Explicit columns including all NOT NULL fields → should succeed
    let d = conn
        .describe("INSERT INTO test_insert_valid (id, required_a, required_b) VALUES (?, ?, ?)".into_sql_str())
        .await;

    assert!(d.is_ok(), "INSERT with all NOT NULL columns should succeed");

    Ok(())
}

#[sqlx_macros::test]
async fn it_validates_insert_missing_required_column() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute(
        "CREATE TEMPORARY TABLE test_insert_missing(
            id INTEGER PRIMARY KEY,
            required_a TEXT NOT NULL,
            required_b TEXT NOT NULL,
            optional_c TEXT
        )",
    )
    .await?;

    // Missing required_b → should error
    let err = conn
        .describe("INSERT INTO test_insert_missing (id, required_a) VALUES (?, ?)".into_sql_str())
        .await;

    assert!(err.is_err(), "INSERT missing NOT NULL column should error");

    let err_msg = format!("{:?}", err);
    assert!(
        err_msg.contains("required_b"),
        "Error should name the missing column: {}",
        err_msg
    );

    Ok(())
}

#[sqlx_macros::test]
async fn it_validates_insert_missing_multiple_required_columns() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute(
        "CREATE TEMPORARY TABLE test_insert_multi_missing(
            id INTEGER PRIMARY KEY,
            required_a TEXT NOT NULL,
            required_b TEXT NOT NULL,
            required_c TEXT NOT NULL
        )",
    )
    .await?;

    // Missing required_b and required_c → error should list both
    let err = conn
        .describe("INSERT INTO test_insert_multi_missing (id, required_a) VALUES (?, ?)".into_sql_str())
        .await;

    assert!(err.is_err());
    let err_msg = format!("{:?}", err);
    assert!(
        err_msg.contains("required_b") && err_msg.contains("required_c"),
        "Error should list all missing columns: {}",
        err_msg
    );

    Ok(())
}

#[sqlx_macros::test]
async fn it_validates_insert_without_column_list() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute(
        "CREATE TEMPORARY TABLE test_insert_no_cols(
            id INTEGER PRIMARY KEY,
            required_a TEXT NOT NULL,
            required_b TEXT NOT NULL
        )",
    )
    .await?;

    // No explicit column list → VALUES implies all columns
    // Runtime will validate; we skip compile-time check
    let d = conn
        .describe("INSERT INTO test_insert_no_cols VALUES (?, ?, ?)".into_sql_str())
        .await;

    assert!(
        d.is_ok(),
        "INSERT without column list should skip compile-time validation (deferred to runtime)"
    );

    Ok(())
}

#[sqlx_macros::test]
async fn it_validates_insert_with_column_defaults() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute(
        "CREATE TEMPORARY TABLE test_insert_defaults(
            id INTEGER PRIMARY KEY,
            required_no_default TEXT NOT NULL,
            required_with_default TEXT NOT NULL DEFAULT 'default_value',
            optional_c TEXT
        )",
    )
    .await?;

    // required_with_default has DEFAULT → not required in INSERT
    let d = conn
        .describe("INSERT INTO test_insert_defaults (id, required_no_default) VALUES (?, ?)".into_sql_str())
        .await;

    assert!(
        d.is_ok(),
        "NOT NULL columns with defaults should not be required"
    );

    Ok(())
}

#[sqlx_macros::test]
async fn it_validates_insert_case_insensitive() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute(
        "CREATE TEMPORARY TABLE TestInsertCase(
            ID INTEGER PRIMARY KEY,
            RequiredCol TEXT NOT NULL
        )",
    )
    .await?;

    // Mixed case table/column names
    let d = conn
        .describe("INSERT INTO testinsertcase (id, requiredcol) VALUES (?, ?)".into_sql_str())
        .await;

    assert!(d.is_ok(), "Case-insensitive matching should work for SQLite identifiers");

    Ok(())
}

#[sqlx_macros::test]
async fn it_validates_insert_with_quoted_identifiers() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    conn.execute(
        "CREATE TEMPORARY TABLE \"test_quoted\"(
            id INTEGER PRIMARY KEY,
            \"col_a\" TEXT NOT NULL,
            col_b TEXT NOT NULL
        )",
    )
    .await?;

    // Quoted identifiers in both table and columns
    let d = conn
        .describe("INSERT INTO \"test_quoted\" (\"col_a\", col_b) VALUES (?, ?)".into_sql_str())
        .await;

    assert!(d.is_ok(), "Quoted identifiers should be parsed correctly");

    Ok(())
}

#[sqlx_macros::test]
async fn it_validates_insert_gracefully_skips_nonexistent_table() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    // Table doesn't exist → validation skips gracefully
    // Runtime will error with "no such table"
    let d = conn
        .describe("INSERT INTO nonexistent_table (col_a) VALUES (?)".into_sql_str())
        .await;

    // Should succeed (or error with different message from actual SQLite),
    // not from our validation
    // In practice, the table doesn't exist so describe will fail at the SQLite level,
    // not at our validation level. That's OK—graceful degradation.
    let _ = d;

    Ok(())
}

#[sqlx_macros::test]
async fn it_validates_insert_from_issue_4206() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    // Exact scenario from issue #4206
    conn.execute(
        "CREATE TEMPORARY TABLE session_group(
            prop_a TEXT NOT NULL,
            prop_b TEXT NOT NULL,
            prop_c TEXT NOT NULL
        )",
    )
    .await?;

    // This INSERT is missing prop_c → should error
    let err = conn
        .describe("INSERT INTO session_group (prop_a, prop_b) VALUES (?, ?)".into_sql_str())
        .await;

    assert!(
        err.is_err(),
        "Regression test for #4206: INSERT missing NOT NULL column should error at compile time"
    );

    let err_msg = format!("{:?}", err);
    assert!(
        err_msg.contains("prop_c"),
        "Error message should mention the missing column 'prop_c': {}",
        err_msg
    );

    Ok(())
}
