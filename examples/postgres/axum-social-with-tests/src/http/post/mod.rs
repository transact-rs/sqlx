use axum::extract::State;
use axum::{Json, Router};

use axum::routing::get;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::http::user::UserAuth;
use sqlx::PgPool;
use validator::Validate;

use crate::http::Result;

use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

mod comment;

pub fn router() -> Router<PgPool> {
    Router::new()
        .route("/v1/post", get(get_posts).post(create_post))
        .merge(comment::router())
}

#[derive(Deserialize, Validate)]
#[serde(rename_all = "camelCase")]
struct CreatePostRequest {
    auth: UserAuth,
    #[validate(length(min = 1, max = 1000))]
    content: String,
}

#[serde_with::serde_as]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Post {
    post_id: Uuid,
    username: String,
    content: String,
    // `OffsetDateTime`'s default serialization format is not standard.
    #[serde_as(as = "Rfc3339")]
    created_at: OffsetDateTime,
}

// #[axum::debug_handler] // very useful!
async fn create_post(db: State<PgPool>, Json(req): Json<CreatePostRequest>) -> Result<Json<Post>> {
    req.validate()?;
    let user_id = req.auth.verify(&*db).await?;

    let post = sqlx::query_as!(
        Post,
        // language=PostgreSQL
        r#"
            with inserted_post as (
                insert into post(user_id, content)
                values ($1, $2)
                returning post_id, user_id, content, created_at
            )
            select post_id, username, content, created_at
            from inserted_post
            inner join "user" using (user_id)
        "#,
        user_id,
        req.content
    )
    .fetch_one(&*db)
    .await?;

    Ok(Json(post))
}

/// Returns posts in descending chronological order.
async fn get_posts(db: State<PgPool>) -> Result<Json<Vec<Post>>> {
    // Note: normally you'd want to put a `LIMIT` on this as well,
    // though that would also necessitate implementing pagination.
    let posts = sqlx::query_as!(
        Post,
        // language=PostgreSQL
        r#"
            select post_id, username, content, created_at
            from post
            inner join "user" using (user_id)
            order by created_at desc
        "#
    )
    .fetch_all(&*db)
    .await?;

    Ok(Json(posts))
}
