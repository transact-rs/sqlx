//! A client certificate whose private key is in a PKCS #11 token, for testing
//! `ssl_client_config` against the `*_client_ssl` servers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use cryptoki::context::{CInitializeArgs, CInitializeFlags, Pkcs11};
use cryptoki::mechanism::eddsa::{EddsaParams, EddsaSignatureScheme};
use cryptoki::mechanism::Mechanism;
use cryptoki::object::{Attribute, ObjectClass, ObjectHandle};
use cryptoki::session::{Session, UserType};
use cryptoki::types::AuthPin;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use rustls::sign::{CertifiedKey, Signer, SigningKey, SingleCertAndKey};
use rustls::{ClientConfig, RootCertStore, SignatureAlgorithm, SignatureScheme};

/// The token, PIN and key label that CI imports `tests/certs/keys/client.key` under.
const TOKEN: &str = "sqlx";
const PIN: &str = "1234";
const KEY: &str = "client";

/// Returns a Rustls client configuration that trusts `tests/certs/ca.crt` and presents
/// `tests/certs/client.crt`, signing with its key in the token of the PKCS #11 module named by
/// `SQLX_TEST_PKCS11_MODULE`, and a count of the signatures the token makes.
///
/// Returns `None` when `SQLX_TEST_PKCS11_MODULE` is not set.
pub fn client_config() -> anyhow::Result<Option<(ClientConfig, Arc<AtomicUsize>)>> {
    let Ok(module) = std::env::var("SQLX_TEST_PKCS11_MODULE") else {
        return Ok(None);
    };

    let pkcs11 = Pkcs11::new(&module).with_context(|| format!("loading {module}"))?;
    pkcs11.initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK))?;
    let slot = pkcs11
        .get_slots_with_token()?
        .into_iter()
        .find(|slot| {
            pkcs11
                .get_token_info(*slot)
                .is_ok_and(|t| t.label() == TOKEN)
        })
        .with_context(|| format!("no token labelled {TOKEN:?}"))?;
    let session = pkcs11.open_ro_session(slot)?;
    session.login(UserType::User, Some(&AuthPin::new(PIN.into())))?;
    let key = *session
        .find_objects(&[
            Attribute::Class(ObjectClass::PRIVATE_KEY),
            Attribute::Label(KEY.into()),
        ])?
        .first()
        .with_context(|| format!("no private key labelled {KEY:?}"))?;

    let signatures = Arc::new(AtomicUsize::new(0));
    let token_key = TokenKey {
        session: Arc::new(Mutex::new(session)),
        key,
        signatures: Arc::clone(&signatures),
    };

    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from_pem_file("tests/certs/ca.crt")?)?;
    let chain = vec![CertificateDer::from_pem_file("tests/certs/client.crt")?];
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_cert_resolver(Arc::new(SingleCertAndKey::from(CertifiedKey::new(
            chain,
            Arc::new(token_key),
        ))));

    Ok(Some((config, signatures)))
}

/// The client's Ed25519 key, used through a logged-in session on the token.
#[derive(Clone, Debug)]
struct TokenKey {
    session: Arc<Mutex<Session>>,
    key: ObjectHandle,
    signatures: Arc<AtomicUsize>,
}

impl SigningKey for TokenKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ED25519)
            .then(|| Box::new(self.clone()) as Box<dyn Signer>)
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ED25519
    }
}

impl Signer for TokenKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let mechanism = Mechanism::Eddsa(EddsaParams::new(EddsaSignatureScheme::Ed25519));
        let session = self
            .session
            .lock()
            .map_err(|_| rustls::Error::General("PKCS #11 session lock poisoned".into()))?;
        let signature = session
            .sign(&mechanism, self.key, message)
            .map_err(|e| rustls::Error::General(format!("PKCS #11 signing failed: {e}")))?;
        self.signatures.fetch_add(1, Ordering::SeqCst);
        Ok(signature)
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}
