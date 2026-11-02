//! PostgreSQL's `oauth` authentication method: SASL OAUTHBEARER, RFC 7628.
//!
//! Every test here is `#[ignore]`d, because none of them can run against a server that
//! authenticates with a password: the mechanism is chosen by the server's `pg_hba.conf`, and
//! a server asking for SCRAM never asks for a token. What they need is the
//! `postgres_18_oauth` service in `tests/docker-compose.yml`, which is a PostgreSQL 18 with
//! the `oauth` method and a validator module that accepts [`TOKEN`]:
//!
//! ```text
//! python3 tests/x.py -t postgres_18_oauth
//! ```
//!
//! or, against an already-running one:
//!
//! ```text
//! cargo test --features postgres,macros --test postgres-oauth -- --include-ignored
//! ```

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{Connection, Error};

/// The token `tests/postgres/oauth/sqlx_oauth_validator.c` accepts.
const TOKEN: &str = "sqlx-test-token";

/// The issuer and scope `tests/postgres/oauth/pg_hba.conf` names, which the server reports
/// in the challenge it returns for a connection it will not authenticate.
const ISSUER: &str = "https://issuer.example.com";
const SCOPE: &str = "openid sqlx";

fn options() -> anyhow::Result<PgConnectOptions> {
    sqlx_test::setup_if_needed();

    Ok(PgConnectOptions::from_str(&std::env::var("DATABASE_URL")?)?)
}

/// The challenge is the only part of an OAuth failure a caller can act on, so every test
/// that expects one goes through here.
fn expect_challenge(error: Error) -> String {
    match error {
        Error::OAuth(challenge) => challenge.document().to_owned(),
        other => panic!("expected an OAuth challenge, got: {other:?}"),
    }
}

#[sqlx_macros::test]
#[ignore = "needs a server using the `oauth` HBA method; see the module docs"]
async fn an_accepted_token_authenticates_the_connection() -> anyhow::Result<()> {
    let options = options()?;
    let mut conn = PgConnection::connect_with(&options.clone().oauth_token(TOKEN)).await?;

    // The bearer token got the connection in, and the password in `DATABASE_URL` played no
    // part: the server asked for OAUTHBEARER, which carries no password.
    let user: String = sqlx::query_scalar("select current_user")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(user, options.get_username());

    Ok(())
}

#[sqlx_macros::test]
#[ignore = "needs a server using the `oauth` HBA method; see the module docs"]
async fn a_connection_without_a_token_is_told_which_one_to_get() -> anyhow::Result<()> {
    // Not a configuration error: the driver has no token, so it asks the server which one it
    // wants. That exchange cannot succeed, and the answer arrives as the error.
    let error = PgConnection::connect_with(&options()?).await.unwrap_err();
    let document = expect_challenge(error);

    // Everything needed to go and get a token: where to ask, and what to ask for.
    assert!(
        document.contains(&format!("{ISSUER}/.well-known/openid-configuration")),
        "challenge does not name the issuer's discovery URI: {document}"
    );
    assert!(
        document.contains(SCOPE),
        "challenge does not name the required scope: {document}"
    );

    Ok(())
}

#[sqlx_macros::test]
#[ignore = "needs a server using the `oauth` HBA method; see the module docs"]
async fn a_rejected_token_is_reported_as_a_challenge_too() -> anyhow::Result<()> {
    // Syntactically a fine bearer token; the validator just doesn't know it. The server
    // answers as it does for a connection with no token at all, so an application whose
    // token expired learns where to get a fresh one.
    let options = options()?.oauth_token("not-the-token-the-server-accepts");

    let error = PgConnection::connect_with(&options).await.unwrap_err();
    let document = expect_challenge(error);

    assert!(
        document.contains("invalid_token"),
        "challenge does not report the token as invalid: {document}"
    );

    Ok(())
}

#[sqlx_macros::test]
#[ignore = "needs a server using the `oauth` HBA method; see the module docs"]
async fn a_token_that_could_forge_a_message_never_reaches_the_server() -> anyhow::Result<()> {
    // The kvsep byte in this token would let the rest of it become key/value pairs of the
    // client's message. The driver checks the token against the RFC 6750 `b64token` grammar
    // before it goes on the wire, so this is a local error rather than a protocol one, and
    // the token stays out of it.
    let options = options()?.oauth_token("token\u{1}host=elsewhere");

    match PgConnection::connect_with(&options).await.unwrap_err() {
        Error::Configuration(error) => {
            assert!(
                !error.to_string().contains("host=elsewhere"),
                "the error quoted the token: {error}"
            );
        }
        other => panic!("expected a configuration error, got: {other:?}"),
    }

    Ok(())
}

#[sqlx_macros::test]
#[ignore = "needs a server using the `oauth` HBA method; see the module docs"]
async fn a_challenge_is_answered_by_dialing_again() -> anyhow::Result<()> {
    // The sequence `PgConnectOptions::oauth_token` documents, end to end. The failed
    // connection is spent: OAUTHBEARER has no way to present a second token on it.
    let options = options()?;

    let options = match PgConnection::connect_with(&options).await {
        Err(Error::OAuth(challenge)) => {
            // Standing in for an OAuth flow against the issuer the challenge names.
            assert!(challenge.document().contains(ISSUER));

            options.oauth_token(TOKEN)
        }
        Ok(_) => panic!("connected without a token; is this server using the `oauth` method?"),
        Err(other) => return Err(other.into()),
    };

    let mut conn = PgConnection::connect_with(&options).await?;

    let one: i32 = sqlx::query_scalar("select 1").fetch_one(&mut conn).await?;

    assert_eq!(1, one);

    Ok(())
}
