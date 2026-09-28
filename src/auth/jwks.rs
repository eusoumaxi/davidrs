//! Bounded JWKS retrieval and refresh.
//!
//! An expired cache or an unknown `kid` triggers a refresh. The key id is fully
//! attacker-controlled: "unknown kid, go fetch" makes every forged token an
//! outbound request. Three bounds prevent that:
//!
//! * **Coalescing.** A [`tokio::sync::Mutex`] serializes refreshes and every
//!   waiter re-checks after acquiring it, so a thousand concurrent misses
//!   cause one fetch.
//! * **A minimum interval.** A refresh that just succeeded is not repeated, so
//!   a sequential flood is bounded too.
//! * **A cooldown after a failure.** A failed refresh is not retried for up to
//!   five seconds, so an unreachable identity provider receives one request
//!   per pause from each instance, not one per incoming token.
//!
//! The key map sits behind a [`std::sync::RwLock`] that is never held across
//! an await.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::RuntimeError;

/// Largest JWKS document this will read, in bytes.
pub const MAX_JWKS_BYTES: usize = 64 * 1024;

/// Largest number of keys kept from one document.
pub const MAX_KEYS: usize = 32;

/// One RSA key from a JWKS document.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Jwk {
    pub(crate) kid: String,
    pub(crate) n: String,
    pub(crate) e: String,
    kty: String,
    alg: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    key_ops: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Document {
    keys: Vec<serde_json::Value>,
}

/// A JWKS cache with a coalesced, rate-bounded refresh.
#[derive(Debug)]
pub(crate) struct KeyStore {
    http: reqwest::Client,
    url: String,
    keys: RwLock<HashMap<String, Jwk>>,
    /// Serializes refreshes. Held across the fetch, while the `keys` lock
    /// never is.
    refreshing: tokio::sync::Mutex<()>,
    /// When the last successful refresh completed.
    refreshed_at: RwLock<Option<Instant>>,
    /// When the last refresh failed, if the most recent one did.
    failed_at: RwLock<Option<Instant>>,
    min_interval: Duration,
    cache_ttl: Duration,
}

/// The longest pause after a failed refresh before another is attempted.
///
/// Short enough that tokens signed with a new key are accepted soon after an
/// outage ends; long enough that an unreachable identity provider receives
/// one request per pause from each instance, not one per incoming token.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(5);

/// Records an in-flight refresh as failed if its future is cancelled, so the
/// cooldown that protects the identity provider still applies after an
/// abandoned refresh. `failed_at` is written on drop only while the download
/// is pending; on normal completion this guard is forgotten so the
/// post-`download` block remains the sole authority on `failed_at`.
struct CancelledAsFailure<'a>(&'a RwLock<Option<Instant>>);

impl Drop for CancelledAsFailure<'_> {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.0.write() {
            *slot = Some(Instant::now());
        }
    }
}

impl KeyStore {
    pub(crate) fn new(
        http: reqwest::Client,
        url: String,
        min_interval: Duration,
        cache_ttl: Duration,
    ) -> Self {
        Self {
            http,
            url,
            keys: RwLock::new(HashMap::new()),
            refreshing: tokio::sync::Mutex::new(()),
            refreshed_at: RwLock::new(None),
            failed_at: RwLock::new(None),
            min_interval: min_interval.min(cache_ttl),
            cache_ttl,
        }
    }

    /// Looks up a key. Takes the read lock only long enough to clone.
    pub(crate) fn get(&self, kid: &str) -> Option<Jwk> {
        if self.since_refresh()? >= self.cache_ttl {
            return None;
        }
        self.keys.read().ok()?.get(kid).cloned()
    }

    /// How long since the last successful refresh, if any.
    fn since_refresh(&self) -> Option<Duration> {
        self.refreshed_at.read().ok()?.map(|at| at.elapsed())
    }

    /// Whether a refresh would be too soon: one succeeded within the minimum
    /// interval, or one failed within the failure cooldown.
    fn too_soon(&self) -> Option<Result<bool, RuntimeError>> {
        if self
            .since_refresh()
            .is_some_and(|elapsed| elapsed < self.min_interval)
        {
            return Some(Ok(false));
        }
        let cooldown = self.min_interval.min(FAILURE_COOLDOWN);
        let failed = self.failed_at.read().ok().and_then(|at| *at);
        failed
            .filter(|at| at.elapsed() < cooldown)
            .map(|_| Err(RuntimeError::message("the JWKS refresh failed moments ago")))
    }

    /// Refreshes unless one succeeded within the minimum interval or failed
    /// within the failure cooldown.
    ///
    /// Both are checked once before queueing on the mutex and again after
    /// acquiring it: the second check is what turns concurrent misses into
    /// one fetch. Counting failed attempts too is what keeps a flood of
    /// unknown key ids from becoming a flood of requests while the identity
    /// provider is down. Returns `true` when a fetch happened.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`] when the document cannot be fetched or parsed,
    /// or when a refresh failed within the cooldown.
    pub(crate) async fn refresh_if_stale(&self) -> Result<bool, RuntimeError> {
        if let Some(answer) = self.too_soon() {
            return answer;
        }
        let _guard = self.refreshing.lock().await;
        if let Some(answer) = self.too_soon() {
            return answer;
        }
        self.fetch().await?;
        Ok(true)
    }

    /// Fetches and replaces the key map. Always performs a request.
    pub(crate) async fn refresh_now(&self) -> Result<(), RuntimeError> {
        let _guard = self.refreshing.lock().await;
        self.fetch().await
    }

    /// Fetches the document and replaces the key map.
    ///
    /// Keeps at most [`MAX_KEYS`] RSA keys compatible with RS256 signature
    /// verification. Other algorithms, encryption keys and keys whose
    /// operations exclude verification are skipped.
    async fn fetch(&self) -> Result<(), RuntimeError> {
        let guard = CancelledAsFailure(&self.failed_at);
        let outcome = self.download().await;
        let failed_at = if outcome.is_err() {
            Some(Instant::now())
        } else {
            None
        };
        if let Ok(mut slot) = self.failed_at.write() {
            *slot = failed_at;
        }
        std::mem::forget(guard);
        outcome
    }

    /// Downloads the document and replaces the key map; see [`KeyStore::fetch`].
    async fn download(&self) -> Result<(), RuntimeError> {
        let response = self
            .http
            .get(&self.url)
            .send()
            .await
            .map_err(|error| crate::client::send_error("fetching the JWKS", error))?;
        if !response.status().is_success() {
            return Err(RuntimeError::message(format!(
                "the JWKS endpoint answered {}",
                response.status()
            )));
        }
        let document: Document = crate::client::json_bounded(response, MAX_JWKS_BYTES).await?;
        let keys: HashMap<String, Jwk> = document
            .keys
            .into_iter()
            .filter_map(|key| serde_json::from_value::<Jwk>(key).ok())
            .filter(|key| {
                key.kty == "RSA"
                    && key.alg.as_deref().is_none_or(|alg| alg == "RS256")
                    && key.usage.as_deref().is_none_or(|usage| usage == "sig")
                    && key
                        .key_ops
                        .as_ref()
                        .is_none_or(|ops| ops.iter().any(|op| op == "verify"))
            })
            .take(MAX_KEYS)
            .map(|key| (key.kid.clone(), key))
            .collect();
        if keys.is_empty() {
            return Err(RuntimeError::message(
                "the JWKS document contained no usable RSA keys",
            ));
        }
        *self
            .keys
            .write()
            .map_err(|_| RuntimeError::message("the JWKS lock is poisoned"))? = keys;
        *self
            .refreshed_at
            .write()
            .map_err(|_| RuntimeError::message("the JWKS lock is poisoned"))? =
            Some(Instant::now());
        Ok(())
    }
}
