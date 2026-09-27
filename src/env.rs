//! Reading the configuration a Lambda receives as environment variables.
//!
//! Configuration is read once, in `main`, before the first invocation; a
//! variable the function cannot run without fails the cold start instead of
//! the first request that needs it. Errors name the variable and never echo
//! its value, because values are often ARNs, URLs or credentials.

use crate::RuntimeError;

/// A variable the function cannot run without.
///
/// # Errors
///
/// Returns [`RuntimeError::Configuration`] naming the variable when it is
/// absent or not valid Unicode.
pub fn required_env(name: &str) -> Result<String, RuntimeError> {
    std::env::var(name)
        .map_err(|_| RuntimeError::Configuration(format!("missing environment variable {name}")))
}

/// A variable that may be absent: `None` when it is unset, empty or blank.
#[must_use]
pub fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// A comma-separated list, each entry trimmed, empty entries dropped.
///
/// Trimming matters: `HTTP_ALLOWED_ORIGINS="https://a, https://b"` must allow
/// `https://b`, not `" https://b"`.
///
/// # Errors
///
/// Returns [`RuntimeError::Configuration`] when the variable is absent.
pub fn list_env(name: &str) -> Result<Vec<String>, RuntimeError> {
    Ok(required_env(name)?
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect())
}
