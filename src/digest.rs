//! SHA-256 and lowercase hex, for identifiers and cache keys.
//!
//! A digest that ends up in a key has to be spelled the same way everywhere it
//! is computed, or two functions derive two keys for one thing. SHA-256 comes
//! from `ring`, which the TLS stack already links.

/// The SHA-256 digest of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = [0_u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// The SHA-256 digest of `data` as lowercase hex.
///
/// # Examples
///
/// ```
/// assert_eq!(
///     davidrs::digest::sha256_hex(b"abc"),
///     "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
/// );
/// ```
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&sha256(data))
}

/// Lowercase hex, two digits per byte.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
