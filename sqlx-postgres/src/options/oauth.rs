use std::fmt::{self, Debug, Formatter};

/// An OAuth 2.0 bearer token, for PostgreSQL's `oauth` authentication method.
///
/// This exists to keep the token out of the `Debug` output of [`PgConnectOptions`], which is
/// derived; see the `Debug` implementation below.
///
/// [`PgConnectOptions`]: crate::PgConnectOptions
#[derive(Clone)]
pub(crate) struct PgOAuthToken(String);

impl PgOAuthToken {
    pub(crate) fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
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
}
