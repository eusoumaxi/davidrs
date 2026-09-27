//! Verified identity — the claims of a token whose signature checked out.
//!
//! This is deliberately *not* an application's user model. It carries no role,
//! no tenant and no permission: those are policy decisions, and a framework
//! that guesses them gets them wrong. Map these claims into your own type.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// The claims of a verified token.
///
/// Construction is crate-private: a value of this type is evidence that
/// [`Verifier::verify`](super::Verifier::verify) succeeded. A downstream crate
/// cannot deserialize one into existence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedClaims {
    claims: Map<String, Value>,
}

impl VerifiedClaims {
    pub(crate) fn new(claims: Map<String, Value>) -> Self {
        Self { claims }
    }

    /// A claim by exact name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.claims.get(name).filter(|value| !value.is_null())
    }

    /// A claim by name, as a string.
    #[must_use]
    pub fn string(&self, name: &str) -> Option<&str> {
        self.get(name)?.as_str()
    }

    /// The `sub` claim — the subject the token was issued for.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.string("sub")
    }

    /// When the token expires: its `exp` claim, which may have a fraction of
    /// a second.
    ///
    /// `None` only for an `exp` that no [`SystemTime`] can represent.
    #[must_use]
    pub fn expires_at(&self) -> Option<SystemTime> {
        let seconds = Duration::try_from_secs_f64(self.get("exp")?.as_f64()?).ok()?;
        UNIX_EPOCH.checked_add(seconds)
    }

    /// All claims, for an application that maps them into its own type.
    #[must_use]
    pub fn all(&self) -> &Map<String, Value> {
        &self.claims
    }
}
