use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::sync::Arc;

use futures_core::future::BoxFuture;
use sqlx_core::error::BoxDynError;

use crate::error::Error;

type Provider = dyn Fn() -> BoxFuture<'static, Result<String, BoxDynError>> + Send + Sync;

/// A source of OAuth 2.0 bearer tokens for PostgreSQL's `oauth` authentication method.
///
/// PostgreSQL 18 added the `oauth` HBA method, which authenticates a connection with an
/// OAuth 2.0 bearer token over the SASL `OAUTHBEARER` mechanism (RFC 7628) instead of a
/// password.
///
/// SQLx does not talk to an identity provider: obtaining a token is the application's job.
/// This type wraps whatever the application already uses to get one.
///
/// Because tokens expire, the token is requested once per connection attempt rather than
/// stored, so a pool that reconnects hours later presents a fresh token. Use
/// [`PgConnectOptions::oauth_token_provider`] for that. A single token that outlives the
/// connections made with it can be set with [`PgConnectOptions::oauth_token`].
///
/// [`PgConnectOptions::oauth_token_provider`]: crate::PgConnectOptions::oauth_token_provider
/// [`PgConnectOptions::oauth_token`]: crate::PgConnectOptions::oauth_token
#[derive(Clone)]
pub struct PgOAuthToken {
    provider: Arc<Provider>,
}

impl PgOAuthToken {
    /// Use a single, fixed token for every connection attempt.
    pub fn new(token: impl Into<String>) -> Self {
        let token = token.into();

        Self::from_provider(move || {
            let token = token.clone();
            async move { Ok(token) }
        })
    }

    /// Call `provider` once per connection attempt to obtain a token.
    pub fn from_provider<F, Fut>(provider: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, BoxDynError>> + Send + 'static,
    {
        Self {
            provider: Arc::new(move || Box::pin(provider())),
        }
    }

    pub(crate) async fn fetch(&self) -> Result<String, Error> {
        (self.provider)().await.map_err(Error::Configuration)
    }
}

/// Deliberately opaque: `PgConnectOptions` derives `Debug`, and a bearer token is a
/// credential that must not reach a log, a panic message or an error.
impl Debug for PgOAuthToken {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("PgOAuthToken(..)")
    }
}

#[cfg(test)]
mod tests {
    use crate::PgConnectOptions;

    #[test]
    fn debug_does_not_leak_the_token() {
        // `PgConnectOptions` derives `Debug`, so the token must be opaque at every depth
        // rather than merely absent from a hand-written summary.
        const TOKEN: &str = "super-secret-bearer-token";

        let options = PgConnectOptions::new_without_pgpass().oauth_token(TOKEN);

        assert!(!format!("{:?}", options).contains(TOKEN));
        assert!(!format!("{:#?}", options).contains(TOKEN));
    }

    #[test]
    fn debug_does_not_leak_a_token_captured_by_a_provider() {
        const TOKEN: &str = "super-secret-bearer-token";

        let options = PgConnectOptions::new_without_pgpass()
            .oauth_token_provider(|| async { Ok(TOKEN.to_string()) });

        assert!(!format!("{:#?}", options).contains(TOKEN));
    }
}
