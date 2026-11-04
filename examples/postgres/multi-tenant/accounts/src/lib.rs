use argon2::{password_hash, Argon2, PasswordHasher, PasswordVerifier};
use password_hash::phc::PasswordHash;
use rand::distr::{Alphanumeric, SampleString};
use sqlx::{Acquire, Executor, PgTransaction, Postgres};
use std::sync::Arc;
use uuid::Uuid;

use tokio::sync::Semaphore;

#[derive(sqlx::Type, Copy, Clone, Debug, serde::Deserialize, serde::Serialize)]
#[sqlx(transparent)]
pub struct AccountId(pub Uuid);

#[derive(sqlx::Type, Clone, Debug, serde::Deserialize, serde::Serialize)]
#[sqlx(transparent)]
pub struct SessionToken(pub String);

pub struct Session {
    pub account_id: AccountId,
    pub session_token: SessionToken,
}

pub struct AccountsManager {
    /// Controls how many blocking tasks are allowed to run concurrently for Argon2 hashing.
    ///
    /// ### Motivation
    /// Tokio blocking tasks are generally not designed for CPU-bound work.
    ///
    /// If no threads are idle, Tokio will automatically spawn new ones to handle
    /// new blocking tasks up to a very high limit--512 by default.
    ///
    /// This is because blocking tasks are expected to spend their time *blocked*, e.g. on
    /// blocking I/O, and thus not consume CPU resources or require a lot of context switching.
    ///
    /// This strategy is not the most efficient way to use threads for CPU-bound work, which
    /// should schedule work to a fixed number of threads to minimize context switching
    /// and memory usage (each new thread needs significant space allocated for its stack).
    ///
    /// We can work around this by using a purpose-designed thread-pool, like Rayon,
    /// but we still have the problem that those APIs usually are not designed to support `async`,
    /// so we end up needing blocking tasks anyway, or implementing our own work queue using
    /// channels. Rayon also does not shut down idle worker threads.
    ///
    /// `block_in_place` is not a silver bullet, either, as it simply uses `spawn_blocking`
    /// internally to take over from the current thread while it is executing blocking work.
    /// This also prevents futures from being polled concurrently in the current task.
    ///
    /// We can lower the limit for blocking threads when creating the runtime, but this risks
    /// starving other blocking tasks that are being created by the application or the Tokio
    /// runtime itself
    /// (which are used for `tokio::fs`, stdio, resolving of hostnames by `ToSocketAddrs`, etc.).
    ///
    /// Instead, we can just use a Semaphore to limit how many blocking tasks are spawned at once,
    /// emulating the behavior of a thread pool like Rayon without needing any additional crates.
    hashing_semaphore: Arc<Semaphore>,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateAccountError {
    #[error("error creating account: email in-use")]
    EmailInUse,
    #[error("error creating account")]
    General(
        #[source]
        #[from]
        GeneralError,
    ),
}

#[derive(Debug, thiserror::Error)]
pub enum CreateSessionError {
    #[error("unknown email")]
    UnknownEmail,
    #[error("invalid password")]
    InvalidPassword,
    #[error("authentication error")]
    General(
        #[source]
        #[from]
        GeneralError,
    ),
}

#[derive(Debug, thiserror::Error)]
pub enum GeneralError {
    #[error("database error")]
    Sqlx(
        #[source]
        #[from]
        sqlx::Error,
    ),
    #[error("error hashing password")]
    PasswordHash(
        #[source]
        #[from]
        password_hash::Error,
    ),
    #[error("task panicked")]
    Task(
        #[source]
        #[from]
        tokio::task::JoinError,
    ),
}

impl AccountsManager {
    pub async fn setup(
        pool: impl Acquire<'_, Database = Postgres>,
        max_hashing_threads: usize,
    ) -> Result<Self, GeneralError> {
        sqlx::migrate!()
            .run(pool)
            .await
            .map_err(sqlx::Error::from)?;

        Ok(AccountsManager {
            hashing_semaphore: Semaphore::new(max_hashing_threads).into(),
        })
    }

    async fn hash_password(&self, password: String) -> Result<PasswordHash, GeneralError> {
        let guard = self
            .hashing_semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("BUG: this semaphore should not be closed");

        // We transfer ownership to the blocking task and back to ensure Tokio doesn't spawn
        // excess threads.
        let (_guard, res) = tokio::task::spawn_blocking(move || {
            (guard, Argon2::default().hash_password(password.as_bytes()))
        })
        .await?;

        Ok(res?)
    }

    async fn verify_password(
        &self,
        password: String,
        hash: PasswordHash,
    ) -> Result<(), CreateSessionError> {
        let guard = self
            .hashing_semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("BUG: this semaphore should not be closed");

        let (_guard, res) = tokio::task::spawn_blocking(move || {
            (
                guard,
                Argon2::default().verify_password(password.as_bytes(), &hash),
            )
        })
        .await
        .map_err(GeneralError::from)?;

        if let Err(password_hash::Error::PasswordInvalid) = res {
            return Err(CreateSessionError::InvalidPassword);
        }

        res.map_err(GeneralError::from)?;

        Ok(())
    }

    pub async fn create(
        &self,
        txn: &mut PgTransaction<'_>,
        email: &str,
        password: String,
    ) -> Result<AccountId, CreateAccountError> {
        // Hash password whether the account exists or not to make it harder
        // to tell the difference in the timing.
        let hash = self.hash_password(password).await?;

        // Thanks to `sqlx.toml`, `account_id` maps to `AccountId`
        sqlx::query_scalar!(
            // language=PostgreSQL
            "insert into accounts.account(email, password_hash) \
             values ($1, $2) \
             returning account_id",
            email,
            // However, since arguments don't link back to the target column,
            // SQLx doesn't know that `PasswordHash` would be a valid argument here.
            hash.to_string(),
        )
        .fetch_one(&mut **txn)
        .await
        .map_err(|e| {
            if e.as_database_error().and_then(|dbe| dbe.constraint())
                == Some("account_account_id_key")
            {
                CreateAccountError::EmailInUse
            } else {
                GeneralError::from(e).into()
            }
        })
    }

    pub async fn create_session(
        &self,
        db: impl Acquire<'_, Database = Postgres>,
        email: &str,
        password: String,
    ) -> Result<Session, CreateSessionError> {
        let mut txn = db.begin().await.map_err(GeneralError::from)?;

        // To save a round-trip to the database, we'll speculatively insert the session token
        // at the same time as we're looking up the password hash.
        //
        // This does nothing until the transaction is actually committed.
        let session_token = SessionToken::generate();

        // Thanks to `sqlx.toml`:
        // * `account_id` maps to `AccountId`
        // * `password_hash` maps to `Text<PasswordHash>`
        // * `session_token` maps to `SessionToken`
        let maybe_account = sqlx::query!(
            // language=PostgreSQL
            "with account as (
                select account_id, password_hash \
                from accounts.account \
                where email = $1
            ), session as (
                insert into accounts.session(session_token, account_id)
                select $2, account_id
                from account
            )
            select account.account_id, account.password_hash from account",
            email,
            session_token.0
        )
        .fetch_optional(&mut *txn)
        .await
        .map_err(GeneralError::from)?;

        let Some(account) = maybe_account else {
            // Hash the password whether the account exists or not to hide the difference in timing.
            self.hash_password(password)
                .await
                .map_err(GeneralError::from)?;
            return Err(CreateSessionError::UnknownEmail);
        };

        self.verify_password(password, account.password_hash.into_inner())
            .await?;

        txn.commit().await.map_err(GeneralError::from)?;

        Ok(Session {
            account_id: account.account_id,
            session_token,
        })
    }

    pub async fn auth_session(
        &self,
        db: impl Executor<'_, Database = Postgres>,
        session_token: &str,
    ) -> Result<Option<AccountId>, GeneralError> {
        sqlx::query_scalar!(
            "select account_id from accounts.session where session_token = $1",
            session_token
        )
        .fetch_optional(db)
        .await
        .map_err(GeneralError::from)
    }
}

impl SessionToken {
    const LEN: usize = 32;

    fn generate() -> Self {
        SessionToken(Alphanumeric.sample_string(&mut rand::rng(), Self::LEN))
    }
}
