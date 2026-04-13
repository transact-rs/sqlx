use axum::http::StatusCode;
use axum::{routing::post, Extension, Json, Router};
use rand::Rng;
use regex::Regex;
use std::{sync::LazyLock, time::Duration};

use serde::Deserialize;
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;
use validator::Validate;

use crate::http::{Error, Result};

pub type UserId = Uuid;

pub fn router() -> Router {
    Router::new().route("/v1/user", post(create_user))
}

static USERNAME_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9A-Za-z_]+$").unwrap());

// CREATE USER

#[derive(Deserialize, Validate)]
#[serde(rename_all = "camelCase")]
pub struct UserAuth {
    #[validate(length(min = 3, max = 16), regex(path = USERNAME_REGEX))]
    username: String,
    #[validate(length(min = 8, max = 32))]
    password: String,
}

// WARNING: this API has none of the checks that a normal user signup flow implements,
// such as email or phone verification.
async fn create_user(db: Extension<PgPool>, Json(req): Json<UserAuth>) -> Result<StatusCode> {
    req.validate()?;

    let UserAuth { username, password } = req;

    // It would be irresponsible to store passwords in plaintext, however.
    let password_hash = crate::password::hash(password).await?;

    sqlx::query!(
        // language=PostgreSQL
        r#"
            insert into "user"(username, password_hash)
            values ($1, $2)
        "#,
        username,
        password_hash
    )
    .execute(&*db)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(dbe) if dbe.constraint() == Some("user_username_key") => {
            Error::Conflict("username taken".into())
        }
        _ => e.into(),
    })?;

    Ok(StatusCode::NO_CONTENT)
}

impl UserAuth {
    // NOTE: normally we wouldn't want to verify the username and password every time,
    // but persistent sessions would have complicated the example.
    pub async fn verify(self, db: impl PgExecutor<'_> + Send) -> Result<UserId> {
        self.validate()?;

        let maybe_user = sqlx::query!(
            r#"select user_id, password_hash from "user" where username = $1"#,
            self.username
        )
        .fetch_optional(db)
        .await?;

        if let Some(user) = maybe_user {
            let verified = crate::password::verify(self.password, user.password_hash).await?;

            if verified {
                return Ok(user.user_id);
            }
        }

        // Sleep a random amount of time to avoid leaking existence of a user in timing.
        let sleep_duration =
            rand::thread_rng().gen_range(Duration::from_millis(100)..=Duration::from_millis(500));
        tokio::time::sleep(sleep_duration).await;

        Err(Error::UnprocessableEntity(
            "invalid username/password".into(),
        ))
    }
}
