// The no-arg variant is covered by other tests already.

use sqlx::{MySqlPool, PgPool};

const PG_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("tests/postgres/migrations");
const MYSQL_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("tests/mysql/migrations");

const PG_USERS_DATABASE_URL: &'static str = "PG_USERS_DATABASE_URL";
const MYSQL_COMMENTS_DATABASE_URL: &'static str = "MYSQL_COMMENTS_DATABASE_URL";

#[sqlx::test(
    env(migrations = "tests/postgres/migrations"), // no database url var
    env(
        migrator = "PG_MIGRATOR",
        fixtures(path = "fixtures/postgres", scripts("users")),
        var = "PG_USERS_DATABASE_URL", // rust path
    ),
    env(
        migrations = "tests/mysql/migrations",
        fixtures(path = "fixtures/mysql", scripts("posts")),
        var("MYSQL_POSTS_DATABASE_URL"), // string literal
    ),
    env(
        migrator = "MYSQL_MIGRATOR",
        fixtures(path = "fixtures/mysql", scripts("comments")),
        var = "MYSQL_COMMENTS_DATABASE_URL", // rust path
    ),
)]
async fn it_gets_from_invidual_environments(
    pg_pool_1: PgPool,
    pg_pool_2: PgPool,
    mysql_pool_1: MySqlPool,
    mysql_pool_2: MySqlPool,
) -> sqlx::Result<()> {
    let db_name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pg_pool_1)
        .await?;

    assert!(db_name.starts_with("_sqlx_test"), "dbname: {db_name:?}");

    let pg_pool_2_usernames: Vec<String> =
        sqlx::query_scalar(r#"SELECT username FROM "user" ORDER BY username"#)
            .fetch_all(&pg_pool_2)
            .await?;

    assert_eq!(pg_pool_2_usernames, ["alice", "bob"]);

    let mysql_pool_1_posts: Vec<String> =
        sqlx::query_scalar("SELECT content FROM post ORDER BY post_id")
            .fetch_all(&mysql_pool_1)
            .await?;

    assert_eq!(
        mysql_pool_1_posts,
        [
            "This new computer is lightning-fast!",
            "@alice is a haxxor :("
        ]
    );

    let mysql_pool_2_comments: Vec<String> =
        sqlx::query_scalar("SELECT content FROM comment WHERE post_id = ? ORDER BY created_at")
            .bind(1)
            .fetch_all(&mysql_pool_2)
            .await?;

    assert_eq!(
        mysql_pool_2_comments,
        ["lol bet ur still bad, 1v1 me", "you're on!"]
    );

    Ok(())
}
