use crate::connection::stream::PgStream;
use crate::error::Error;
use crate::message::{OAuthBearerResponse, SaslInitialResponse, SaslResponse};
use crate::options::PgOAuthToken;

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

/// Authenticate with a bearer token over SASL `OAUTHBEARER`.
///
/// This is the "token-first" flow: the token travels in the SASL initial client response, so a
/// successful authentication costs no extra round trip. SQLx never contacts the identity
/// provider, so the discovery flow of RFC 7628 §3.2.2 is not implemented; if the server rejects
/// the token, the exchange is closed out and the server's error is returned.
pub(crate) async fn authenticate(stream: &mut PgStream, token: &PgOAuthToken) -> Result<(), Error> {
    let token = token.fetch().await?;
    let response = initial_client_response(&token)?;

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

            // `PgStream::recv` turns `ErrorResponse` into `Err`, which is the expected
            // outcome here and carries the server's own diagnostic.
            stream.recv().await?;

            // Reached only if the server said something else entirely.
            Err(err_protocol!(
                "OAUTHBEARER authentication failed; server returned: {}",
                String::from_utf8_lossy(&document)
            ))
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
    use super::initial_client_response;

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
