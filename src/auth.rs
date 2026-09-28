//! Bearer-token verification: RS256 signatures checked against a JWKS that is
//! refreshed in a bounded, coalesced way.
//!
//! The scope is narrow on purpose. A [`Verifier`] proves that a token was
//! signed by the configured issuer and satisfies the configured claim rules,
//! and stops there. Roles, tenants and permissions are the application's; see
//! [`VerifiedClaims`].
//!
//! It handles the parts that are easy to get wrong: `alg` is pinned to RS256,
//! the token and the JWKS document are size-bounded, and an unknown `kid`
//! cannot drive a burst of requests at the identity provider.

mod claims;
mod jwks;
mod verify;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use claims::VerifiedClaims;
pub use jwks::{MAX_JWKS_BYTES, MAX_KEYS};
pub use verify::{MAX_TOKEN_BYTES, VerifyError};

use crate::RuntimeError;

/// How a verifier decides whether a token is acceptable.
///
/// Build it with [`VerifierConfig::new`] and the methods below.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifierConfig {
    /// The exact value the `iss` claim must have.
    pub issuer: String,
    /// Where to fetch the signing keys.
    pub jwks_url: String,
    /// Accepted audiences: a token's `aud`, or its `client_id` when it has no
    /// `aud`, must be one of them.
    ///
    /// Empty refuses every token unless [`VerifierConfig::any_audience`] is
    /// set: a token issued for another application of the same issuer must
    /// not be accepted by accident.
    pub audiences: Vec<String>,
    /// Accept a token for any audience. Off by default; see
    /// [`VerifierConfig::without_audience_check`].
    pub any_audience: bool,
    /// Claims that must equal an exact string, e.g. `("token_use", "id")`.
    pub required_claims: Vec<(String, String)>,
    /// Clock skew allowed when checking `exp` and `nbf`.
    pub leeway: Duration,
    /// Shortest interval between two successful JWKS refreshes.
    ///
    /// It bounds how often an unknown `kid` can cause a fetch.
    pub min_refresh_interval: Duration,
}

impl VerifierConfig {
    /// A configuration with 60 s of leeway, at most one key refresh per
    /// minute and no required claims.
    ///
    /// It accepts no token until the audiences are set with
    /// [`with_audiences`](VerifierConfig::with_audiences): the audience check
    /// is what stops a token issued for a different application of the same
    /// issuer.
    #[must_use]
    pub fn new(issuer: impl Into<String>, jwks_url: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            jwks_url: jwks_url.into(),
            audiences: Vec::new(),
            any_audience: false,
            required_claims: Vec::new(),
            leeway: Duration::from_secs(60),
            min_refresh_interval: Duration::from_secs(60),
        }
    }

    /// Accepts tokens whose `aud` — or, when a token has no `aud`, whose
    /// `client_id` — is one of these, as an API Gateway JWT authorizer does.
    ///
    /// Amazon Cognito access tokens carry no `aud` unless resource binding is
    /// on, so their app client id is matched through `client_id`.
    #[must_use]
    pub fn with_audiences(mut self, audiences: Vec<String>) -> Self {
        self.audiences = audiences;
        self
    }

    /// Accepts a token issued for any audience of the issuer.
    ///
    /// Only for an issuer that issues tokens to this application alone, and
    /// never for a shared identity provider: any other application's tokens
    /// then work here too.
    #[must_use]
    pub fn without_audience_check(mut self) -> Self {
        self.any_audience = true;
        self
    }

    /// Requires a claim to equal an exact value.
    #[must_use]
    pub fn with_required_claim(
        mut self,
        claim: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.required_claims.push((claim.into(), value.into()));
        self
    }

    /// Sets the shortest interval between key refreshes.
    #[must_use]
    pub fn with_min_refresh_interval(mut self, interval: Duration) -> Self {
        self.min_refresh_interval = interval;
        self
    }
}

/// Verifies RS256 tokens against a cached JWKS.
///
/// Build one per process, in `main`, and share it through the application
/// state: the key cache lives as long as the verifier.
///
/// # Examples
///
/// ```no_run
/// use davidrs::auth::{Verifier, VerifierConfig};
/// use davidrs::client::{self, Limits};
///
/// # async fn run(authorization: &str) -> Result<(), davidrs::RuntimeError> {
/// let config = VerifierConfig::new(
///     "https://id.example.com",
///     "https://id.example.com/.well-known/jwks.json",
/// )
/// .with_audiences(vec!["web".to_owned()]);
/// let verifier = Verifier::load(client::build(Limits::default())?, config).await?;
///
/// let token = davidrs::auth::bearer(authorization);
/// if let Ok(claims) = verifier.verify(token).await {
///     println!("signed in as {:?}", claims.subject());
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Verifier {
    config: VerifierConfig,
    keys: jwks::KeyStore,
}

impl Verifier {
    /// Builds a verifier and loads the key set once.
    ///
    /// The load is async and explicit, so the network call is visible where
    /// the verifier is built, and a broken configuration fails the cold start
    /// instead of the first request.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`] when the key set cannot be fetched, is larger
    /// than [`MAX_JWKS_BYTES`], is not a JWKS document, or holds no usable RSA
    /// key.
    pub async fn load(http: reqwest::Client, config: VerifierConfig) -> Result<Self, RuntimeError> {
        let keys = jwks::KeyStore::new(http, config.jwks_url.clone(), config.min_refresh_interval);
        keys.refresh_now().await?;
        Ok(Self { config, keys })
    }

    /// Builds a verifier without loading keys.
    ///
    /// The first verification fetches the key set. Use it when a cold start
    /// should not pay for a JWKS request it may not need.
    #[must_use]
    pub fn deferred(http: reqwest::Client, config: VerifierConfig) -> Self {
        let keys = jwks::KeyStore::new(http, config.jwks_url.clone(), config.min_refresh_interval);
        Self { config, keys }
    }

    /// Verifies a token and returns its claims.
    ///
    /// An unknown `kid` is either a key rotation or a forged token. It causes
    /// at most one refresh, shared by concurrent callers and skipped when the
    /// last refresh is younger than [`VerifierConfig::min_refresh_interval`],
    /// so neither case can flood the identity provider.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError`]. Map every variant to the same 401: the
    /// distinction is for logs, not for clients.
    pub async fn verify(&self, token: &str) -> Result<VerifiedClaims, VerifyError> {
        let (encoded_header, encoded_payload, encoded_signature) = verify::split(token)?;
        let header = verify::header(encoded_header)?;
        let kid = header.kid.ok_or(VerifyError::Malformed)?;

        let key = match self.keys.get(&kid) {
            Some(key) => key,
            None => {
                self.keys
                    .refresh_if_stale()
                    .await
                    .map_err(|_| VerifyError::UnknownKey)?;
                self.keys.get(&kid).ok_or(VerifyError::UnknownKey)?
            }
        };

        let signed = &token[..encoded_header.len() + 1 + encoded_payload.len()];
        verify::signature(&key.n, &key.e, signed, encoded_signature)?;

        let claims = verify::payload(encoded_payload)?;
        self.check_claims(&claims)?;
        Ok(VerifiedClaims::new(claims))
    }

    /// Checks `iss`, the required claims, `aud`, `exp` and, when present,
    /// `nbf`.
    ///
    /// Times are JSON numbers of seconds and may have a fraction (RFC 7519's
    /// `NumericDate`), so they are compared as floating point: an absurd
    /// value reads as far future or long past and never wraps around into the
    /// opposite verdict. A `nbf` that is not a number makes the token
    /// unacceptable rather than ignored.
    fn check_claims(
        &self,
        claims: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), VerifyError> {
        if claims.get("iss").and_then(serde_json::Value::as_str)
            != Some(self.config.issuer.as_str())
        {
            return Err(VerifyError::Claims);
        }
        for (name, expected) in &self.config.required_claims {
            if claims.get(name).and_then(serde_json::Value::as_str) != Some(expected.as_str()) {
                return Err(VerifyError::Claims);
            }
        }
        if !self.config.any_audience && !verify::audience_matches(claims, &self.config.audiences) {
            return Err(VerifyError::Claims);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |since| since.as_secs_f64());
        let leeway = self.config.leeway.as_secs_f64();
        let expiry = claims
            .get("exp")
            .and_then(serde_json::Value::as_f64)
            .ok_or(VerifyError::Claims)?;
        if expiry + leeway <= now {
            return Err(VerifyError::Claims);
        }
        if let Some(not_before) = claims.get("nbf") {
            let not_before = not_before.as_f64().ok_or(VerifyError::Claims)?;
            if not_before - leeway > now {
                return Err(VerifyError::Claims);
            }
        }
        Ok(())
    }
}

/// The token in an `Authorization` header value.
///
/// Strips a `Bearer` scheme, matched case-insensitively as RFC 6750 allows,
/// and the surrounding whitespace.
///
/// # Examples
///
/// ```
/// assert_eq!(davidrs::auth::bearer("Bearer eyJ.eyJ.c2ln"), "eyJ.eyJ.c2ln");
/// assert_eq!(davidrs::auth::bearer("bearer  eyJ.eyJ.c2ln "), "eyJ.eyJ.c2ln");
/// assert_eq!(davidrs::auth::bearer("eyJ.eyJ.c2ln"), "eyJ.eyJ.c2ln");
/// ```
#[must_use]
pub fn bearer(authorization: &str) -> &str {
    let value = authorization.trim();
    match value.split_once(char::is_whitespace) {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim(),
        _ => value,
    }
}
