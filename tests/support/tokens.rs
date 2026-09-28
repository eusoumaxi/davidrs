//! RS256 tokens signed with a throwaway key, and a JWKS endpoint that
//! publishes it, for tests that verify tokens end to end.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use davidrs::auth::{Verifier, VerifierConfig};
use davidrs::client::{self, Limits};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents};
use serde_json::{Value, json};

use super::server::{self, Server};

/// The issuer every test token names.
pub const ISSUER: &str = "https://id.example.com";

/// JWT base64: URL-safe, unpadded.
pub const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The key every test token is signed with.
///
/// A throwaway RSA-2048 key generated with `openssl genpkey` for these tests
/// only. It is published on purpose, protects nothing, and must never sign
/// anything real.
pub fn test_key() -> RsaKeyPair {
    RsaKeyPair::from_pkcs8(include_bytes!(
        "../fixtures/throwaway-test-only-rsa-2048.pk8"
    ))
    .expect("the test key is PKCS#8")
}

/// The JWKS entry of the test key under `kid`.
pub fn jwk(kid: &str) -> Value {
    let public = RsaPublicKeyComponents::<Vec<u8>>::from(test_key().public());
    json!({ "kty": "RSA", "kid": kid, "n": B64.encode(public.n), "e": B64.encode(public.e) })
}

/// An RS256 header naming `kid`.
pub fn rs256(kid: &str) -> Value {
    json!({ "alg": "RS256", "kid": kid })
}

/// Seconds since the epoch, as a JWT counts time.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs() as i64
}

/// A token over already-encoded header and payload, signed with the test key.
pub fn signed(header: &str, payload: &str) -> String {
    let key = test_key();
    let message = format!("{header}.{payload}");
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .expect("sign");
    format!("{message}.{}", B64.encode(signature))
}

/// A token with this header and these claims, signed with the test key.
pub fn token(header: &Value, claims: &Value) -> String {
    signed(
        &B64.encode(header.to_string()),
        &B64.encode(claims.to_string()),
    )
}

/// The outbound client the verifiers use.
pub fn http() -> reqwest::Client {
    client::build(Limits::default()).expect("client")
}

/// A JWKS endpoint at `/jwks` that counts its fetches.
///
/// Each answer takes 50 ms, so concurrent callers pile up behind the first
/// fetch the way they do behind a real identity provider.
pub struct Jwks {
    /// The server publishing the document.
    pub server: Server,
    keys: Arc<Mutex<Vec<Value>>>,
}

impl Jwks {
    /// Publishes `keys` at `/jwks`.
    pub async fn serving(keys: Vec<Value>) -> Self {
        let keys = Arc::new(Mutex::new(keys));
        let published = Arc::clone(&keys);
        let server = Server::start(move |_| {
            let document = json!({ "keys": *published.lock().expect("keys") });
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                server::json(&document)
            }
        })
        .await;
        Self { server, keys }
    }

    /// Replaces the published keys, as a key rotation does.
    pub fn publish(&self, keys: Vec<Value>) {
        *self.keys.lock().expect("keys") = keys;
    }

    /// How many times the document was fetched.
    pub fn fetches(&self) -> usize {
        self.server.hits("/jwks")
    }

    /// The issuer, audience `web` and `token_use = id`.
    pub fn config(&self) -> VerifierConfig {
        VerifierConfig::new(ISSUER, self.server.url("/jwks"))
            .with_audiences(vec!["web".to_owned()])
            .with_required_claim("token_use", "id")
    }

    /// A verifier for `config`, with its keys loaded.
    pub async fn load(&self, config: VerifierConfig) -> Verifier {
        Verifier::load(http(), config).await.expect("load")
    }

    /// A verifier for [`Jwks::config`], with its keys loaded.
    pub async fn verifier(&self) -> Verifier {
        self.load(self.config()).await
    }
}
