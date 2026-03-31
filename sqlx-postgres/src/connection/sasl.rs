use std::sync::Arc;

use async_lock::{RwLock, RwLockUpgradableReadGuard};

use crate::connection::stream::PgStream;
use crate::error::Error;
use crate::message::{
    Authentication, AuthenticationSasl, AuthenticationSaslContinue, SaslInitialResponse,
    SaslResponse,
};
use crate::rt;
use crate::PgConnectOptions;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::{Digest, Sha256};
use stringprep::saslprep;

use base64::prelude::{Engine as _, BASE64_STANDARD};

const GS2_HEADER: &str = "n,,";
const CHANNEL_ATTR: &str = "c";
const USERNAME_ATTR: &str = "n";
const CLIENT_PROOF_ATTR: &str = "p";
const NONCE_ATTR: &str = "r";

/// A single-entry cache for the client key derived from the password.
///
/// Salting the password and deriving the client key can be expensive, so this cache stores the most
/// recently used client key along with the parameters used to derive it.
///
/// The cache is keyed on the salt and iteration count, which are the server-provided parameters
/// that affect the HMAC result. The password is not included in the cache key because it can only
/// change via `&mut self` on `PgConnectOptions`, which replaces the cache instance.
///
/// An async `RwLock` is used so that only one caller computes the key at a time; subsequent callers
/// wait and then read the cached result.
///
/// According to [RFC-7677](https://datatracker.ietf.org/doc/html/rfc7677):
///
/// > This computational cost can be avoided by caching the ClientKey (assuming the Salt and hash
/// > iteration-count is stable).
#[derive(Debug, Clone)]
pub struct ClientKeyCache {
    inner: Arc<RwLock<Option<CacheEntry>>>,
}

#[derive(Debug)]
struct CacheEntry {
    // Keys
    salt: Vec<u8>,
    iterations: u32,

    // Values
    salted_password: [u8; 32],
    client_key: Hmac<Sha256>,
}

impl CacheEntry {
    fn matches(&self, cont: &AuthenticationSaslContinue) -> bool {
        self.salt == cont.salt && self.iterations == cont.iterations
    }
}

impl ClientKeyCache {
    pub fn new() -> Self {
        ClientKeyCache {
            inner: Arc::new(RwLock::new(None)),
        }
    }

    /// Returns the cached salted password and client key HMAC if the cache matches the given
    /// salt and iteration count. Otherwise, computes and caches them.
    async fn get_or_compute(
        &self,
        password: &str,
        cont: &AuthenticationSaslContinue,
    ) -> Result<([u8; 32], Hmac<Sha256>), Error> {
        let guard = self.inner.upgradable_read().await;

        if let Some(entry) = guard.as_ref().filter(|e| e.matches(cont)) {
            return Ok((entry.salted_password, entry.client_key.clone()));
        }

        let mut guard = RwLockUpgradableReadGuard::upgrade(guard).await;

        // Re-check after acquiring the write lock, in case another caller populated the cache.
        if let Some(entry) = guard.as_ref().filter(|e| e.matches(cont)) {
            return Ok((entry.salted_password, entry.client_key.clone()));
        }

        // SaltedPassword := Hi(Normalize(password), salt, i)
        let salted_password = hi(password, &cont.salt, cont.iterations).await?;

        // ClientKey := HMAC(SaltedPassword, "Client Key")
        let client_key =
            Hmac::<Sha256>::new_from_slice(&salted_password).map_err(Error::protocol)?;

        *guard = Some(CacheEntry {
            salt: cont.salt.clone(),
            iterations: cont.iterations,
            salted_password,
            client_key: client_key.clone(),
        });

        Ok((salted_password, client_key))
    }
}

pub(crate) async fn authenticate(
    stream: &mut PgStream,
    options: &PgConnectOptions,
    data: AuthenticationSasl,
) -> Result<(), Error> {
    let mut has_sasl = false;
    let mut has_sasl_plus = false;
    let mut unknown = Vec::new();

    for mechanism in data.mechanisms() {
        match mechanism {
            "SCRAM-SHA-256" => {
                has_sasl = true;
            }

            "SCRAM-SHA-256-PLUS" => {
                has_sasl_plus = true;
            }

            _ => {
                unknown.push(mechanism.to_owned());
            }
        }
    }

    if !has_sasl_plus && !has_sasl {
        return Err(err_protocol!(
            "unsupported SASL authentication mechanisms: {}",
            unknown.join(", ")
        ));
    }

    // channel-binding = "c=" base64
    let mut channel_binding = format!("{CHANNEL_ATTR}=");
    BASE64_STANDARD.encode_string(GS2_HEADER, &mut channel_binding);

    // "n=" saslname ;; Usernames are prepared using SASLprep.
    let username = format!("{}={}", USERNAME_ATTR, options.username);
    let username = match saslprep(&username) {
        Ok(v) => v,
        // TODO(danielakhterov): Remove panic when we have proper support for configuration errors
        Err(_) => panic!("Failed to saslprep username"),
    };

    // nonce = "r=" c-nonce [s-nonce] ;; Second part provided by server.
    let nonce = gen_nonce();

    // client-first-message-bare = [reserved-mext ","] username "," nonce ["," extensions]
    let client_first_message_bare = format!("{username},{nonce}");

    let client_first_message = format!("{GS2_HEADER}{client_first_message_bare}");

    stream
        .send(SaslInitialResponse {
            response: &client_first_message,
            plus: false,
        })
        .await?;

    let cont = match stream.recv_expect().await? {
        Authentication::SaslContinue(data) => data,

        auth => {
            return Err(err_protocol!(
                "expected SASLContinue but received {:?}",
                auth
            ));
        }
    };

    let (salted_password, mut mac) = options
        .sasl_client_key_cache
        .get_or_compute(options.password.as_deref().unwrap_or_default(), &cont)
        .await?;

    mac.update(b"Client Key");

    let client_key = mac.finalize().into_bytes();

    // StoredKey := H(ClientKey)
    let stored_key = Sha256::digest(client_key);

    // client-final-message-without-proof
    let client_final_message_wo_proof = format!(
        "{channel_binding},r={nonce}",
        channel_binding = channel_binding,
        nonce = &cont.nonce
    );

    // AuthMessage := client-first-message-bare + "," + server-first-message + "," + client-final-message-without-proof
    let auth_message = format!(
        "{client_first_message_bare},{server_first_message},{client_final_message_wo_proof}",
        client_first_message_bare = client_first_message_bare,
        server_first_message = cont.message,
        client_final_message_wo_proof = client_final_message_wo_proof
    );

    // ClientSignature := HMAC(StoredKey, AuthMessage)
    let mut mac = Hmac::<Sha256>::new_from_slice(&stored_key).map_err(Error::protocol)?;
    mac.update(auth_message.as_bytes());

    let client_signature = mac.finalize().into_bytes();

    // ClientProof := ClientKey XOR ClientSignature
    let client_proof: Vec<u8> = client_key
        .iter()
        .zip(client_signature.iter())
        .map(|(&a, &b)| a ^ b)
        .collect();

    // ServerKey := HMAC(SaltedPassword, "Server Key")
    let mut mac = Hmac::<Sha256>::new_from_slice(&salted_password).map_err(Error::protocol)?;
    mac.update(b"Server Key");

    let server_key = mac.finalize().into_bytes();

    // ServerSignature := HMAC(ServerKey, AuthMessage)
    let mut mac = Hmac::<Sha256>::new_from_slice(&server_key).map_err(Error::protocol)?;
    mac.update(auth_message.as_bytes());

    // client-final-message = client-final-message-without-proof "," proof
    let mut client_final_message = format!("{client_final_message_wo_proof},{CLIENT_PROOF_ATTR}=");
    BASE64_STANDARD.encode_string(client_proof, &mut client_final_message);

    stream.send(SaslResponse(&client_final_message)).await?;

    let data = match stream.recv_expect().await? {
        Authentication::SaslFinal(data) => data,

        auth => {
            return Err(err_protocol!("expected SASLFinal but received {:?}", auth));
        }
    };

    // authentication is only considered valid if this verification passes
    mac.verify_slice(&data.verifier).map_err(Error::protocol)?;

    Ok(())
}

// nonce is a sequence of random printable bytes
fn gen_nonce() -> String {
    let mut rng = rand::thread_rng();
    let count = rng.gen_range(64..128);

    // printable = %x21-2B / %x2D-7E
    // ;; Printable ASCII except ",".
    // ;; Note that any "printable" is also
    // ;; a valid "value".
    let nonce: String = std::iter::repeat(())
        .map(|()| {
            let mut c = rng.gen_range(0x21u8..0x7F);

            while c == 0x2C {
                c = rng.gen_range(0x21u8..0x7F);
            }

            c
        })
        .take(count)
        .map(|c| c as char)
        .collect();

    rng.gen_range(32..128);
    format!("{NONCE_ATTR}={nonce}")
}

// Hi(str, salt, i):
async fn hi<'a>(s: &'a str, salt: &'a [u8], iter_count: u32) -> Result<[u8; 32], Error> {
    let mut mac = Hmac::<Sha256>::new_from_slice(s.as_bytes()).map_err(Error::protocol)?;

    mac.update(salt);
    mac.update(&1u32.to_be_bytes());

    let mut u = mac.finalize_reset().into_bytes();
    let mut hi = u;

    for i in 1..iter_count {
        mac.update(u.as_slice());
        u = mac.finalize_reset().into_bytes();
        hi = hi.iter().zip(u.iter()).map(|(&a, &b)| a ^ b).collect();

        // For large iteration counts, this process can take a long time and block the event loop.
        // It was measured as taking ~50ms for 4096 iterations (the default) on a developer machine.
        // If we want to yield every 10-100us (as generally advised for tokio), then we can yield
        // every 5 iterations which should be every ~50us.
        if i % 5 == 0 {
            rt::yield_now().await;
        }
    }

    Ok(hi.into())
}
