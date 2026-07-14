use sqlx::{query_file, query_file_as, FromRow, PgPool};
use std::fmt::{Display, Formatter};

#[derive(FromRow)]
struct PostWithAuthorQuery {
    pub post_id: i64,
    pub title: String,
    pub body: String,
    pub author_id: i64,
    pub author_username: String,
}

impl Display for PostWithAuthorQuery {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            r#"
            post_id: {},
            title: {},
            body: {},
            author_id: {},
            author_username: {}
        "#,
            self.post_id, self.title, self.body, self.author_id, self.author_username
        )
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let pool = PgPool::connect(&dotenvy::var("DATABASE_URL")?).await?;

    // we can use a traditional wrapper around the `query!()` macro using files
    query_file!("queries/insert_seed_data.sql")
        .execute(&pool)
        .await?;

    // we can also use `query_file_as!()` similarly to `query_as!()` to map our database models
    let posts_with_authors = query_file_as!(PostWithAuthorQuery, "queries/list_all_posts.sql")
        .fetch_all(&pool)
        .await?;

    for post_with_author in posts_with_authors {
        println!("{post_with_author}");
    }

    Ok(())
}
