//! Token parsing and RS256 signature checks.
//!
//! Uses `ring`, which the TLS stack already links, instead of a JOSE crate and
//! a second crypto provider. RS256 is the only algorithm.

use base64::Engine;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Largest token this will parse, in bytes.
///
/// A token is a header, a payload and a 256-byte signature; anything far
/// larger is not worth decoding.
pub const MAX_TOKEN_BYTES: usize = 8 * 1024;

/// Why verification failed.
///
/// Coarse on purpose: telling a client *which* check failed helps it tune a
/// forgery. Log the variant and answer every one with the same 401.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// The token was not a well-formed RS256 JWT, or was too long.
    #[error("the token is malformed")]
    Malformed,
    /// No key matched the token's `kid`, even after a refresh.
    #[error("no key matched the token")]
    UnknownKey,
    /// The signature did not verify.
    #[error("the signature is invalid")]
    Signature,
    /// A required claim was missing, wrong or expired.
    #[error("the claims are not acceptable")]
    Claims,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Header {
    pub(crate) alg: String,
    pub(crate) kid: Option<String>,
}

pub(crate) const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Splits a token into its three parts, rejecting anything else.
pub(crate) fn split(token: &str) -> Result<(&str, &str, &str), VerifyError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(VerifyError::Malformed);
    }
    let mut parts = token.split('.');
    let (header, payload, signature) = (
        parts.next().ok_or(VerifyError::Malformed)?,
        parts.next().ok_or(VerifyError::Malformed)?,
        parts.next().ok_or(VerifyError::Malformed)?,
    );
    if parts.next().is_some() || header.is_empty() || payload.is_empty() || signature.is_empty() {
        return Err(VerifyError::Malformed);
    }
    Ok((header, payload, signature))
}

/// Reads the JOSE header, requiring RS256.
///
/// `alg` is attacker-controlled: pinning it is what stops `none` and
/// HMAC-with-the-public-key confusion.
pub(crate) fn header(encoded: &str) -> Result<Header, VerifyError> {
    let bytes = B64.decode(encoded).map_err(|_| VerifyError::Malformed)?;
    let header: Header = serde_json::from_slice(&bytes).map_err(|_| VerifyError::Malformed)?;
    if header.alg != "RS256" {
        return Err(VerifyError::Malformed);
    }
    Ok(header)
}

/// Checks an RS256 signature against RSA components.
pub(crate) fn signature(
    n: &str,
    e: &str,
    signed: &str,
    encoded_signature: &str,
) -> Result<(), VerifyError> {
    let components = ring::signature::RsaPublicKeyComponents {
        n: B64.decode(n).map_err(|_| VerifyError::UnknownKey)?,
        e: B64.decode(e).map_err(|_| VerifyError::UnknownKey)?,
    };
    let signature = B64
        .decode(encoded_signature)
        .map_err(|_| VerifyError::Malformed)?;
    components
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            signed.as_bytes(),
            &signature,
        )
        .map_err(|_| VerifyError::Signature)
}

/// Decodes the payload.
pub(crate) fn payload(encoded: &str) -> Result<Map<String, Value>, VerifyError> {
    let bytes = B64.decode(encoded).map_err(|_| VerifyError::Malformed)?;
    serde_json::from_slice(&bytes).map_err(|_| VerifyError::Malformed)
}

/// Whether `aud` names one of the accepted audiences, or, for a token with no
/// `aud`, its `client_id` does. Nothing matches an empty list.
pub(crate) fn audience_matches(claims: &Map<String, Value>, accepted: &[String]) -> bool {
    let is_accepted = |audience: &str| accepted.iter().any(|candidate| candidate == audience);
    match claims.get("aud") {
        Some(Value::String(audience)) => is_accepted(audience),
        Some(Value::Array(audiences)) => {
            audiences.iter().filter_map(Value::as_str).any(is_accepted)
        }
        Some(_) => false,
        None => claims
            .get("client_id")
            .and_then(Value::as_str)
            .is_some_and(is_accepted),
    }
}
