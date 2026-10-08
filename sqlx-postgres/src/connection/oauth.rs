use crate::connection::stream::PgStream;
use crate::error::Error;
use crate::message::{OAuthBearerResponse, SaslInitialResponse, SaslResponse};

/// The only SASL mechanism PostgreSQL's `oauth` HBA method advertises.
///
/// OAUTHBEARER defines no channel binding, so there is no `-PLUS` variant to negotiate.
pub(crate) const MECHANISM: &str = "OAUTHBEARER";

/// RFC 7628 key/value separator (`kvsep`).
const KVSEP: &str = "\x01";

/// RFC 5801 gs2-header. OAUTHBEARER has no channel binding and PostgreSQL rejects the `p`
/// specifier outright, so this is always `n` (client does not support channel binding)
/// followed by an empty authzid.
const GS2_HEADER: &str = "n,,";

const BEARER_SCHEME: &str = "Bearer ";

/// Authenticate over SASL `OAUTHBEARER`, presenting `token` if there is one.
///
/// With a token this is the "token-first" flow: the token travels in the SASL initial client
/// response, so a successful authentication costs no extra round trip.
///
/// Without one the exchange is still worth making, because it is how the caller finds out
/// which token to get: an empty `auth` value asks the server for its OAuth parameters, and it
/// answers with the status document that [`Error::OAuth`] carries back out. That exchange
/// cannot succeed by design, and neither can the one where the server rejects a token, so in
/// both cases the caller obtains a token and dials again. SQLx never contacts an identity
/// provider itself.
pub(crate) async fn authenticate(stream: &mut PgStream, token: Option<&str>) -> Result<(), Error> {
    let response = match token {
        Some(token) => initial_client_response(token)?,
        None => discovery_client_response(),
    };

    stream
        .send(SaslInitialResponse {
            mechanism: MECHANISM,
            response: &response,
        })
        .await?;

    match stream.recv_expect::<OAuthBearerResponse>().await? {
        // The server validated the token. It sends no mechanism-specific final data, so
        // `AuthenticationOk` arrives directly and the exchange is over.
        OAuthBearerResponse::Ok => Ok(()),

        OAuthBearerResponse::Failure(document) => {
            // RFC 7628 §3.2.3: the only response the server will accept now is a single
            // kvsep. Sending it lets the server report the failure as a normal
            // `ErrorResponse` instead of leaving the exchange hanging.
            stream.send(SaslResponse(KVSEP)).await?;

            match stream.recv().await {
                // The expected outcome, and the reason the document is what we return: the
                // server's own error says only that authentication failed. It names neither
                // the issuer nor the scope, so there is nothing in it for a caller that
                // needs to go and get a token.
                Err(Error::Database(_)) => {}

                // Anything else went wrong on the way, and is not about the token.
                Err(error) => return Err(error),

                // The server has no other move here; if it made one, we no longer know what
                // state the connection is in.
                Ok(message) => {
                    return Err(err_protocol!(
                        "expected an error to close out the failed OAUTHBEARER exchange, \
                         received {:?}",
                        message.format
                    ));
                }
            }

            Err(Error::oauth(String::from_utf8_lossy(&document)))
        }
    }
}

/// Build the initial client response: `n,,^Aauth=Bearer <token>^A^A`.
fn initial_client_response(token: &str) -> Result<String, Error> {
    validate_token(token)?;

    Ok(format!(
        "{GS2_HEADER}{KVSEP}auth={BEARER_SCHEME}{token}{KVSEP}{KVSEP}"
    ))
}

/// Build a request for the server's OAuth parameters: `n,,^Aauth=^A^A`.
///
/// A completely empty `auth` value is how RFC 7628 §4.3 asks a server which issuer and scope
/// a token needs; PostgreSQL answers it with the same status document it returns for a
/// rejected token, and then fails the exchange.
fn discovery_client_response() -> String {
    format!("{GS2_HEADER}{KVSEP}auth={KVSEP}{KVSEP}")
}

/// Check the token against the `b64token` grammar of RFC 6750 §2.1, which is what the server
/// itself enforces.
///
/// This is not merely a nicety. The grammar excludes the kvsep byte, whitespace and NUL, so a
/// token that passes cannot forge additional key/value pairs or truncate the message. Checking
/// it here turns a corrupt token into a clear local error instead of a protocol violation from
/// the server.
///
/// The token value is never included in the error.
fn validate_token(token: &str) -> Result<(), Error> {
    // Tokens may end with any number of base64 padding characters.
    let unpadded = token.trim_end_matches('=');

    if unpadded.is_empty() {
        return Err(Error::Configuration(
            "OAuth bearer token is empty".to_string().into(),
        ));
    }

    if !unpadded.bytes().all(is_b64token_byte) {
        return Err(Error::Configuration(
            "OAuth bearer token contains characters that are not allowed by the `b64token` \
             grammar of RFC 6750; the token value is omitted from this error"
                .to_string()
                .into(),
        ));
    }

    Ok(())
}

const fn is_b64token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
}

#[cfg(test)]
mod tests {
    use super::{discovery_client_response, initial_client_response};

    #[test]
    fn initial_client_response_is_token_first() {
        // The token travels in the initial response, so no challenge is needed first.
        assert_eq!(
            initial_client_response("abc123").unwrap(),
            "n,,\x01auth=Bearer abc123\x01\x01"
        );
    }

    #[test]
    fn initial_client_response_declares_no_channel_binding() {
        // PostgreSQL rejects the `p` specifier for OAUTHBEARER outright.
        let response = initial_client_response("abc123").unwrap();

        assert!(response.starts_with("n,,"));
        assert!(!response.starts_with('p'));
    }

    #[test]
    fn discovery_response_carries_an_empty_auth_value() {
        // Not `auth=Bearer `: the scheme is left out entirely, which is what the server
        // reads as a request for its OAuth parameters rather than as a malformed token.
        assert_eq!(discovery_client_response(), "n,,\x01auth=\x01\x01");
    }

    #[test]
    fn padded_token_is_accepted() {
        assert_eq!(
            initial_client_response("dG9rZW4=").unwrap(),
            "n,,\x01auth=Bearer dG9rZW4=\x01\x01"
        );
    }

    #[test]
    fn b64token_alphabet_is_accepted() {
        initial_client_response("aZ09-._~+/").unwrap();
    }

    // The rejection cases go through `initial_client_response` rather than calling the
    // check directly, so that dropping the check from the call site fails a test.

    #[test]
    fn token_may_not_forge_a_key_value_pair() {
        // Without this check the kvsep would let a token append its own kvpairs.
        initial_client_response("abc\x01host=evil").unwrap_err();
    }

    #[test]
    fn token_may_not_contain_nul_or_whitespace() {
        // The server compares the message length against `strlen`, so a NUL is fatal.
        initial_client_response("abc\0def").unwrap_err();
        initial_client_response("abc def").unwrap_err();
        initial_client_response("abc\ndef").unwrap_err();
    }

    #[test]
    fn empty_token_is_rejected() {
        // An empty token must not be turned into a discovery request by accident: the
        // caller asked to authenticate with a token, so this is a configuration error.
        initial_client_response("").unwrap_err();
        initial_client_response("==").unwrap_err();
    }

    #[test]
    fn an_error_never_contains_the_token() {
        const TOKEN: &str = "sensitive\x01value";

        let error = initial_client_response(TOKEN).unwrap_err().to_string();

        assert!(!error.contains("sensitive"));
    }
}
