//! SHA-256 and hex, checked against published test vectors.
#![cfg(feature = "digest")]

use davidrs::digest::{hex, sha256, sha256_hex};

#[test]
fn sha256_of_the_empty_input_matches_the_published_vector() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_hex_is_the_hex_of_sha256() {
    assert_eq!(sha256_hex(b"order-1"), hex(&sha256(b"order-1")));
}

#[test]
fn hex_is_lowercase_and_zero_padded() {
    assert_eq!(hex(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
    assert_eq!(hex(&[]), "");
}
