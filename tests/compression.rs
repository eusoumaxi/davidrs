//! Bounded gzip: the decoded size is capped whatever the compressed size.
#![cfg(feature = "compression")]

use davidrs::RuntimeError;
use davidrs::compression::{gunzip_bounded, gunzip_to_string_bounded, gzip};

fn packed(data: &[u8]) -> Vec<u8> {
    gzip(data).expect("gzip")
}

fn is_limit(error: &RuntimeError, expected: usize) -> bool {
    matches!(error, RuntimeError::LimitExceeded { kind: "decoded bytes", limit } if *limit == expected as u64)
}

#[test]
fn a_round_trip_returns_the_original_bytes() {
    let data = b"order-1,order-2,order-3\n".repeat(50);
    assert_eq!(
        gunzip_bounded(&packed(&data), 10_000).expect("gunzip"),
        data
    );
}

#[test]
fn a_payload_exactly_at_the_limit_is_accepted() {
    let decoded = gunzip_bounded(&packed(&[7; 1000]), 1000).expect("at the cap");
    assert_eq!(decoded.len(), 1000);
}

#[test]
fn one_byte_over_the_limit_is_refused() {
    let error = gunzip_bounded(&packed(&[7; 1001]), 1000).expect_err("over the cap");
    assert!(is_limit(&error, 1000), "{error}");
}

/// 16 MiB of zeros compress to a few kilobytes; decoding stops at the cap
/// instead of materializing the whole expansion.
#[test]
fn a_bomb_is_refused_rather_than_truncated() {
    let bomb = packed(&vec![0; 16 * 1024 * 1024]);
    assert!(bomb.len() < 64 * 1024, "{} bytes", bomb.len());
    let error = gunzip_bounded(&bomb, 64 * 1024).expect_err("refused");
    assert!(is_limit(&error, 64 * 1024), "{error}");
}

#[test]
fn the_largest_limit_decodes_normally() {
    assert_eq!(
        gunzip_bounded(&packed(b"abc"), usize::MAX).expect("no practical cap"),
        b"abc"
    );
}

#[test]
fn input_that_is_not_gzip_is_an_error() {
    let error = gunzip_bounded(b"definitely not gzip", 1024).expect_err("not gzip");
    assert!(error.to_string().starts_with("gunzip"), "{error}");
}

#[test]
fn a_string_round_trip_preserves_utf8() {
    let text = "café, naïve, 日本語";
    let decoded = gunzip_to_string_bounded(&packed(text.as_bytes()), 1024).expect("utf-8");
    assert_eq!(decoded, text);
}

#[test]
fn a_string_that_is_not_utf8_is_an_error() {
    let error = gunzip_to_string_bounded(&packed(&[0xff, 0xfe]), 1024).expect_err("not utf-8");
    assert_eq!(error.to_string(), "gunzip utf-8");
}

#[test]
fn the_string_variant_applies_the_same_cap() {
    let error = gunzip_to_string_bounded(&packed(b"abcd"), 3).expect_err("over the cap");
    assert!(is_limit(&error, 3), "{error}");
}

/// Concatenated gzip members are one stream: every member is decoded, and
/// the cap applies to all of them together.
#[test]
fn every_member_of_a_multi_member_stream_is_decoded() {
    let mut both = packed(b"first ");
    both.extend(packed(b"second"));
    assert_eq!(
        gunzip_bounded(&both, 64).expect("both members"),
        b"first second"
    );
    assert!(
        gunzip_bounded(&both, 8).is_err(),
        "the cap covers the whole stream"
    );
}
