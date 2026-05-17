use axum::extract::{Path, State};
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

pub fn router() -> Router<PgPool> {
    Router::new().route(
        "/v1/post/{postId}/comment",
        get(get_post_comments).post(create_post_comment),
    )
}

#[derive(Deserialize, Validate)]
#[serde(rename_all = "camelCase")]
struct CreateCommentRequest {
    auth: UserAuth,
    #[validate(length(min = 1, max = 1000))]
    content: String,
}

#[serde_with::serde_as]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    comment_id: Uuid,
    username: String,
    content: String,
    // `OffsetDateTime`'s default serialization format is not standard.
    #[serde_as(as = "Rfc3339")]
    created_at: OffsetDateTime,
}

// #[axum::debug_handler] // very useful!
async fn create_post_comment(
    db: State<PgPool>,
    Path(post_id): Path<Uuid>,
    Json(req): Json<CreateCommentRequest>,
) -> Result<Json<Comment>> {
    req.validate()?;
    let user_id = req.auth.verify(&*db).await?;

    let comment = sqlx::query_as!(
        Comment,
        // language=PostgreSQL
        r#"
            with inserted_comment as (
                insert into comment(user_id, post_id, content)
                values ($1, $2, $3)
                returning comment_id, user_id, content, created_at
            )
            select comment_id, username, content, created_at
            from inserted_comment
            inner join "user" using (user_id)
        "#,
        user_id,
        post_id,
        req.content
    )
    .fetch_one(&*db)
    .await?;

    Ok(Json(comment))
}

/// Returns comments in ascending chronological order.
async fn get_post_comments(
    db: State<PgPool>,
    Path(post_id): Path<Uuid>,
) -> Result<Json<Vec<Comment>>> {
    // Note: normally you'd want to put a `LIMIT` on this as well,
    // though that would also necessitate implementing pagination.
    let comments = sqlx::query_as!(
        Comment,
        // language=PostgreSQL
        r#"
            select comment_id, username, content, created_at
            from comment
            inner join "user" using (user_id)
            where post_id = $1
            order by created_at
        "#,
        post_id
    )
    .fetch_all(&*db)
    .await?;

    Ok(Json(comments))
}
