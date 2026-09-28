//! Token verification through [`Verifier`], end to end.
//!
//! Tokens are signed with a throwaway RSA key and checked against a JWKS
//! served from a local port, so every rule is exercised the way a deployed
//! function meets it: over HTTP, with real signatures, counting the fetches
//! the identity provider would see.
#![cfg(feature = "auth")]

mod support;

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use davidrs::RuntimeError;
use davidrs::auth::{
    MAX_JWKS_BYTES, MAX_KEYS, MAX_TOKEN_BYTES, Verifier, VerifierConfig, VerifyError, bearer,
};
use serde_json::{Value, json};
use support::server::Server;
use support::tokens::{B64, ISSUER, Jwks, http, jwk, now, rs256, signed, token};

/// Claims every check of [`Jwks::config`] accepts.
fn claims() -> Value {
    json!({
        "iss": ISSUER,
        "sub": "user-1",
        "aud": "web",
        "token_use": "id",
        "exp": now() + 3600,
    })
}

/// [`claims`] with one claim replaced, or removed when `value` is null.
fn claims_with(name: &str, value: Value) -> Value {
    let mut claims = claims();
    let object = claims.as_object_mut().expect("an object");
    if value.is_null() {
        object.remove(name);
    } else {
        object.insert(name.to_owned(), value);
    }
    claims
}

/// A valid token signed under `kid`.
fn valid(kid: &str) -> String {
    token(&rs256(kid), &claims())
}

async fn refused(verifier: &Verifier, token: &str) -> VerifyError {
    verifier.verify(token).await.expect_err("refused")
}

async fn load_error(status: u16, content_type: &'static str, body: String) -> RuntimeError {
    let server = Server::fixed(status, content_type, body).await;
    Verifier::load(http(), VerifierConfig::new(ISSUER, server.url("/jwks")))
        .await
        .expect_err("load fails")
}

#[test]
fn a_new_config_allows_a_minute_of_leeway_and_one_refresh_a_minute() {
    let config = VerifierConfig::new(ISSUER, "https://id.example.com/jwks");
    assert_eq!(config.issuer, ISSUER);
    assert_eq!(config.jwks_url, "https://id.example.com/jwks");
    assert!(config.audiences.is_empty());
    assert!(config.required_claims.is_empty());
    assert_eq!(config.leeway, Duration::from_secs(60));
    assert_eq!(config.min_refresh_interval, Duration::from_secs(60));
}

#[test]
fn the_builder_methods_set_audiences_required_claims_and_the_refresh_interval() {
    let config = VerifierConfig::new(ISSUER, "https://id.example.com/jwks")
        .with_audiences(vec!["web".to_owned()])
        .with_required_claim("token_use", "id")
        .with_required_claim("scope", "orders")
        .with_min_refresh_interval(Duration::from_secs(5));
    assert_eq!(config.audiences, ["web"]);
    assert_eq!(
        config.required_claims,
        [
            ("token_use".to_owned(), "id".to_owned()),
            ("scope".to_owned(), "orders".to_owned())
        ]
    );
    assert_eq!(config.min_refresh_interval, Duration::from_secs(5));
}

#[tokio::test]
async fn load_fetches_the_key_set_once_and_verification_reuses_it() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    assert_eq!(jwks.fetches(), 1);
    verifier.verify(&valid("k1")).await.expect("valid");
    verifier.verify(&valid("k1")).await.expect("valid");
    assert_eq!(jwks.fetches(), 1);
}

#[tokio::test]
async fn a_deferred_verifier_fetches_the_key_set_on_first_use() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = Verifier::deferred(http(), jwks.config());
    assert_eq!(jwks.fetches(), 0);
    verifier.verify(&valid("k1")).await.expect("valid");
    assert_eq!(jwks.fetches(), 1);
}

#[tokio::test]
async fn a_deferred_verifier_whose_keys_cannot_be_fetched_reports_an_unknown_key() {
    let jwks = Jwks::serving(Vec::new()).await;
    let verifier = Verifier::deferred(http(), jwks.config());
    assert_eq!(
        refused(&verifier, &valid("k1")).await,
        VerifyError::UnknownKey
    );
}

#[tokio::test]
async fn load_fails_when_the_endpoint_is_unreachable() {
    let config = VerifierConfig::new(ISSUER, "http://127.0.0.1:1/jwks");
    Verifier::load(http(), config)
        .await
        .expect_err("nothing listens on port 1");
}

#[tokio::test]
async fn load_fails_when_the_endpoint_answers_an_error_status() {
    let error = load_error(500, "application/json", jwk_document(&[jwk("k1")])).await;
    assert!(error.to_string().contains("500"), "{error}");
}

#[tokio::test]
async fn load_fails_when_the_document_is_larger_than_max_jwks_bytes() {
    let padding = "x".repeat(MAX_JWKS_BYTES);
    let body = json!({ "keys": [jwk("k1")], "padding": padding }).to_string();
    let error = load_error(200, "application/json", body).await;
    assert!(
        matches!(error, RuntimeError::LimitExceeded { limit, .. } if limit == MAX_JWKS_BYTES as u64),
        "{error}"
    );
}

#[tokio::test]
async fn load_fails_when_the_document_is_not_a_jwks() {
    let error = load_error(200, "application/json", r#"{"keys":"none"}"#.to_owned()).await;
    assert!(matches!(error, RuntimeError::Other { .. }), "{error}");
}

#[tokio::test]
async fn load_fails_when_no_key_is_rsa() {
    let ec = json!({ "kty": "EC", "kid": "ec1", "crv": "P-256", "x": "AQAB", "y": "AQAB" });
    let error = load_error(200, "application/json", jwk_document(&[ec])).await;
    assert!(error.to_string().contains("no usable RSA keys"), "{error}");
}

fn jwk_document(keys: &[Value]) -> String {
    json!({ "keys": keys }).to_string()
}

/// Identity providers publish elliptic-curve and symmetric keys next to RSA
/// ones; those entries have no `n` or `e` and must not break the document.
#[tokio::test]
async fn keys_that_are_not_rsa_are_ignored_next_to_rsa_ones() {
    let jwks = Jwks::serving(vec![
        json!({ "kty": "EC", "kid": "ec1", "crv": "P-256", "x": "AQAB", "y": "AQAB" }),
        json!({ "kty": "oct", "kid": "hmac1", "k": "c2VjcmV0" }),
        json!({ "kty": "EC", "kid": "ec2", "n": "AQAB", "e": "AQAB" }),
        jwk("k1"),
    ])
    .await;
    let verifier = jwks.verifier().await;
    verifier.verify(&valid("k1")).await.expect("the RSA key");
    assert_eq!(
        refused(&verifier, &valid("ec2")).await,
        VerifyError::UnknownKey
    );
}

#[tokio::test]
async fn a_key_without_kty_is_used_as_rsa() {
    let mut key = jwk("k1");
    key.as_object_mut().expect("object").remove("kty");
    let jwks = Jwks::serving(vec![key]).await;
    jwks.verifier()
        .await
        .verify(&valid("k1"))
        .await
        .expect("valid");
}

#[tokio::test]
async fn only_the_first_max_keys_are_kept() {
    let keys = (0..=MAX_KEYS)
        .map(|index| jwk(&format!("k{index}")))
        .collect();
    let jwks = Jwks::serving(keys).await;
    let verifier = jwks.verifier().await;
    verifier.verify(&valid("k0")).await.expect("the first key");
    let last = format!("k{MAX_KEYS}");
    assert_eq!(
        refused(&verifier, &valid(&last)).await,
        VerifyError::UnknownKey
    );
}

#[tokio::test]
async fn a_published_key_that_is_not_base64url_cannot_verify_anything() {
    let jwks = Jwks::serving(vec![
        json!({ "kty": "RSA", "kid": "bad-n", "n": "!!", "e": "AQAB" }),
        json!({ "kty": "RSA", "kid": "bad-e", "n": "AQAB", "e": "!!" }),
    ])
    .await;
    let verifier = jwks.verifier().await;
    assert_eq!(
        refused(&verifier, &valid("bad-n")).await,
        VerifyError::UnknownKey
    );
    assert_eq!(
        refused(&verifier, &valid("bad-e")).await,
        VerifyError::UnknownKey
    );
}

#[tokio::test]
async fn a_valid_token_yields_its_claims() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let expiry = now() + 3600;
    let token = token(&rs256("k1"), &claims_with("exp", json!(expiry)));
    let claims = jwks.verifier().await.verify(&token).await.expect("valid");
    assert_eq!(claims.subject(), Some("user-1"));
    assert_eq!(
        claims.expires_at(),
        Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(expiry).expect("after 1970")))
    );
    assert_eq!(claims.string("token_use"), Some("id"));
    assert_eq!(claims.get("aud"), Some(&json!("web")));
    assert_eq!(claims.all()["iss"], ISSUER);
}

#[tokio::test]
async fn an_expiry_beyond_system_time_reads_as_none() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let token = token(&rs256("k1"), &claims_with("exp", json!(1e300)));
    let claims = jwks.verifier().await.verify(&token).await.expect("valid");
    assert_eq!(claims.expires_at(), None);
}

#[tokio::test]
async fn verified_claims_treat_null_as_absent_and_their_accessors_check_the_type() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let mut body = claims_with("sub", json!(42));
    body["nickname"] = Value::Null;
    let claims = jwks
        .verifier()
        .await
        .verify(&token(&rs256("k1"), &body))
        .await
        .expect("valid");
    assert_eq!(claims.get("nickname"), None);
    assert!(claims.all().contains_key("nickname"));
    assert_eq!(claims.get("sub"), Some(&json!(42)));
    assert_eq!(claims.subject(), None);
    assert_eq!(claims.string("missing"), None);
}

#[tokio::test]
async fn a_wrong_issuer_is_refused() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let token = token(
        &rs256("k1"),
        &claims_with("iss", json!("https://other.example.com")),
    );
    assert_eq!(
        refused(&jwks.verifier().await, &token).await,
        VerifyError::Claims
    );
}

#[tokio::test]
async fn a_wrong_audience_is_refused() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let token = token(&rs256("k1"), &claims_with("aud", json!("mobile")));
    assert_eq!(
        refused(&jwks.verifier().await, &token).await,
        VerifyError::Claims
    );
}

#[tokio::test]
async fn an_audience_array_is_accepted_when_it_holds_an_accepted_audience() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let token = token(
        &rs256("k1"),
        &claims_with("aud", json!(["mobile", 7, "web"])),
    );
    jwks.verifier()
        .await
        .verify(&token)
        .await
        .expect("web is accepted");
}

#[tokio::test]
async fn an_audience_array_without_an_accepted_audience_is_refused() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let token = token(&rs256("k1"), &claims_with("aud", json!(["mobile", "tv"])));
    assert_eq!(
        refused(&jwks.verifier().await, &token).await,
        VerifyError::Claims
    );
}

#[tokio::test]
async fn a_missing_or_non_string_audience_is_refused() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    for audience in [Value::Null, json!(7)] {
        let token = token(&rs256("k1"), &claims_with("aud", audience));
        assert_eq!(refused(&verifier, &token).await, VerifyError::Claims);
    }
}

#[tokio::test]
async fn a_required_claim_with_another_value_is_refused() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    for token_use in [json!("access"), Value::Null] {
        let token = token(&rs256("k1"), &claims_with("token_use", token_use));
        assert_eq!(refused(&verifier, &token).await, VerifyError::Claims);
    }
}

#[tokio::test]
async fn an_expired_token_is_refused() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let token = token(&rs256("k1"), &claims_with("exp", json!(now() - 3600)));
    assert_eq!(
        refused(&jwks.verifier().await, &token).await,
        VerifyError::Claims
    );
}

#[tokio::test]
async fn an_expiry_inside_the_leeway_is_accepted_and_one_past_it_is_not() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let mut strict = jwks.config();
    strict.leeway = Duration::ZERO;
    let just_expired = token(&rs256("k1"), &claims_with("exp", json!(now() - 30)));
    jwks.verifier()
        .await
        .verify(&just_expired)
        .await
        .expect("inside the default 60 s of leeway");
    assert_eq!(
        refused(&jwks.load(strict).await, &just_expired).await,
        VerifyError::Claims
    );
}

#[tokio::test]
async fn a_token_without_a_numeric_exp_is_refused() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    for expiry in [Value::Null, json!("tomorrow"), json!(true)] {
        let token = token(&rs256("k1"), &claims_with("exp", expiry));
        assert_eq!(refused(&verifier, &token).await, VerifyError::Claims);
    }
}

/// The leeway is added with saturation, so the extremes cannot wrap around
/// into the opposite verdict.
#[tokio::test]
async fn the_extreme_expiries_keep_their_meaning() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    let far_future = token(&rs256("k1"), &claims_with("exp", json!(i64::MAX)));
    verifier.verify(&far_future).await.expect("far future");
    let long_past = token(&rs256("k1"), &claims_with("exp", json!(i64::MIN)));
    assert_eq!(refused(&verifier, &long_past).await, VerifyError::Claims);
}

#[tokio::test]
async fn alg_none_is_refused() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let unsigned = format!(
        "{}.{}.c2ln",
        B64.encode(json!({ "alg": "none", "kid": "k1" }).to_string()),
        B64.encode(claims().to_string())
    );
    assert_eq!(
        refused(&jwks.verifier().await, &unsigned).await,
        VerifyError::Malformed
    );
}

/// A token that names HS256 is refused before its signature is looked at,
/// which stops a forgery that uses the public key as an HMAC secret.
#[tokio::test]
async fn alg_hs256_is_refused() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let token = token(&json!({ "alg": "HS256", "kid": "k1" }), &claims());
    assert_eq!(
        refused(&jwks.verifier().await, &token).await,
        VerifyError::Malformed
    );
}

#[tokio::test]
async fn a_token_that_is_not_three_non_empty_parts_is_malformed() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    let parts: Vec<String> = valid("k1").split('.').map(str::to_owned).collect();
    let (header, payload, signature) = (&parts[0], &parts[1], &parts[2]);
    for shape in [
        String::new(),
        format!("{header}.{payload}"),
        format!("{header}.{payload}.{signature}.{signature}"),
        format!("{header}..{signature}"),
        format!(".{payload}.{signature}"),
        format!("{header}.{payload}."),
    ] {
        assert_eq!(
            refused(&verifier, &shape).await,
            VerifyError::Malformed,
            "{shape:.40}"
        );
    }
}

#[tokio::test]
async fn a_token_of_exactly_max_token_bytes_is_accepted() {
    let jwks = Jwks::serving(vec![jwk("k01")]).await;
    let header = B64.encode(rs256("k01").to_string());
    let signature_len = B64.encode([0_u8; 256]).len();
    let payload = (0..MAX_TOKEN_BYTES)
        .map(|pad| B64.encode(claims_with("pad", json!("x".repeat(pad))).to_string()))
        .find(|payload| header.len() + payload.len() + signature_len + 2 == MAX_TOKEN_BYTES)
        .expect("some padding reaches the exact size");
    let token = signed(&header, &payload);
    assert_eq!(token.len(), MAX_TOKEN_BYTES);
    jwks.verifier()
        .await
        .verify(&token)
        .await
        .expect("at the cap");
}

#[tokio::test]
async fn a_token_over_max_token_bytes_is_refused_before_any_key_fetch() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = Verifier::deferred(http(), jwks.config());
    let token = token(
        &rs256("k1"),
        &claims_with("pad", json!("x".repeat(MAX_TOKEN_BYTES))),
    );
    assert_eq!(refused(&verifier, &token).await, VerifyError::Malformed);
    assert_eq!(jwks.fetches(), 0);
}

#[tokio::test]
async fn a_header_that_is_not_base64url_json_with_a_kid_is_malformed() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    let payload = B64.encode(claims().to_string());
    for header in [
        "!!".to_owned(),
        B64.encode("not json"),
        B64.encode(json!({ "alg": "RS256" }).to_string()),
    ] {
        let token = signed(&header, &payload);
        assert_eq!(refused(&verifier, &token).await, VerifyError::Malformed);
    }
}

#[tokio::test]
async fn a_signature_that_is_not_base64url_is_malformed() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let valid = valid("k1");
    let unsigned = valid.rsplit_once('.').expect("three parts").0;
    assert_eq!(
        refused(&jwks.verifier().await, &format!("{unsigned}.!!")).await,
        VerifyError::Malformed
    );
}

#[tokio::test]
async fn a_tampered_payload_fails_the_signature_check() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let valid = valid("k1");
    let (header, rest) = valid.split_once('.').expect("header");
    let signature = rest.split_once('.').expect("payload").1;
    let forged = B64.encode(claims_with("sub", json!("admin")).to_string());
    assert_eq!(
        refused(
            &jwks.verifier().await,
            &format!("{header}.{forged}.{signature}")
        )
        .await,
        VerifyError::Signature
    );
}

#[tokio::test]
async fn a_signed_payload_that_is_not_a_base64url_json_object_is_malformed() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    let header = B64.encode(rs256("k1").to_string());
    for payload in ["!!".to_owned(), B64.encode("[1]")] {
        let token = signed(&header, &payload);
        assert_eq!(refused(&verifier, &token).await, VerifyError::Malformed);
    }
}

#[tokio::test]
async fn a_rotated_key_is_picked_up_with_one_refresh() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks
        .load(jwks.config().with_min_refresh_interval(Duration::ZERO))
        .await;
    jwks.publish(vec![jwk("k1"), jwk("k2")]);
    verifier.verify(&valid("k2")).await.expect("the new key");
    assert_eq!(jwks.fetches(), 2);
}

/// Forged tokens with made-up key ids, sent concurrently.
async fn flood(verifier: &Arc<Verifier>, count: usize) -> Vec<VerifyError> {
    let tasks: Vec<_> = (0..count)
        .map(|index| {
            let verifier = Arc::clone(verifier);
            let token = valid(&format!("forged-{index}"));
            tokio::spawn(async move { verifier.verify(&token).await })
        })
        .collect();
    let mut errors = Vec::new();
    for task in tasks {
        errors.push(task.await.expect("task").expect_err("unknown key"));
    }
    errors
}

#[tokio::test]
async fn concurrent_unknown_key_ids_cause_exactly_one_fetch() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = Arc::new(Verifier::deferred(http(), jwks.config()));
    let errors = flood(&verifier, 32).await;
    assert!(errors.iter().all(|error| *error == VerifyError::UnknownKey));
    assert_eq!(jwks.fetches(), 1);
}

#[tokio::test]
async fn unknown_key_ids_inside_the_minimum_interval_cause_no_fetch() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = Arc::new(jwks.verifier().await);
    flood(&verifier, 32).await;
    assert_eq!(jwks.fetches(), 1, "only the load fetched");
}

#[test]
fn bearer_returns_the_token_with_or_without_the_scheme() {
    assert_eq!(bearer("Bearer abc"), "abc");
    assert_eq!(bearer("Bearer  abc "), "abc");
    assert_eq!(bearer(" abc "), "abc");
}

/// RFC 6750 schemes are case-insensitive; another scheme is left untouched.
#[test]
fn bearer_matches_the_scheme_in_any_case() {
    assert_eq!(bearer("bearer abc"), "abc");
    assert_eq!(bearer("BEARER\tabc"), "abc");
    assert_eq!(bearer("Basic dXNlcg=="), "Basic dXNlcg==");
}

/// `exp` and `nbf` are `NumericDate`s: seconds that may have a fraction.
#[tokio::test]
async fn fractional_times_are_accepted() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    let mut claims = claims_with("exp", json!(now() as f64 + 3600.5));
    claims["nbf"] = json!(now() as f64 - 0.25);
    verifier
        .verify(&token(&rs256("k1"), &claims))
        .await
        .expect("fractional seconds are valid");
}

#[tokio::test]
async fn a_token_used_before_its_nbf_is_refused_outside_the_leeway() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    let early = token(&rs256("k1"), &claims_with("nbf", json!(now() + 3600)));
    assert_eq!(refused(&verifier, &early).await, VerifyError::Claims);
    let within_leeway = token(&rs256("k1"), &claims_with("nbf", json!(now() + 30)));
    verifier
        .verify(&within_leeway)
        .await
        .expect("inside the minute of leeway");
    let unreadable = token(&rs256("k1"), &claims_with("nbf", json!("soon")));
    assert_eq!(refused(&verifier, &unreadable).await, VerifyError::Claims);
}

/// While the key set cannot be fetched, unknown key ids cause one fetch per
/// cooldown, not one per token; once it recovers, a new key is picked up.
#[tokio::test]
async fn a_failing_key_set_is_retried_after_a_cooldown_not_per_token() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks
        .load(
            jwks.config()
                .with_min_refresh_interval(Duration::from_millis(200)),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    jwks.publish(Vec::new());
    assert_eq!(
        refused(&verifier, &valid("k2")).await,
        VerifyError::UnknownKey
    );
    assert_eq!(jwks.fetches(), 2, "the load, then one failed refresh");
    for _ in 0..20 {
        assert_eq!(
            refused(&verifier, &valid("k2")).await,
            VerifyError::UnknownKey
        );
    }
    assert_eq!(jwks.fetches(), 2, "no fetch inside the cooldown");
    tokio::time::sleep(Duration::from_millis(250)).await;
    jwks.publish(vec![jwk("k2")]);
    verifier
        .verify(&valid("k2"))
        .await
        .expect("the rotated key after recovery");
    assert_eq!(jwks.fetches(), 3);
}

#[test]
fn every_verify_error_reads_as_a_short_sentence() {
    for (error, text) in [
        (VerifyError::Malformed, "the token is malformed"),
        (VerifyError::UnknownKey, "no key matched the token"),
        (VerifyError::Signature, "the signature is invalid"),
        (VerifyError::Claims, "the claims are not acceptable"),
    ] {
        assert_eq!(error.to_string(), text);
    }
}

/// Without accepted audiences, no token passes: a token issued for another
/// application of the same issuer must not work by accident.
#[tokio::test]
async fn a_config_without_audiences_refuses_every_token() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks
        .load(
            VerifierConfig::new(ISSUER, jwks.server.url("/jwks"))
                .with_required_claim("token_use", "id"),
        )
        .await;
    assert_eq!(refused(&verifier, &valid("k1")).await, VerifyError::Claims);
}

#[tokio::test]
async fn any_audience_is_accepted_only_when_asked_for() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let config = VerifierConfig::new(ISSUER, jwks.server.url("/jwks")).without_audience_check();
    assert!(config.any_audience);
    let verifier = jwks.load(config).await;
    let elsewhere = token(&rs256("k1"), &claims_with("aud", json!("another-app")));
    verifier.verify(&elsewhere).await.expect("any audience");
}

/// As an API Gateway JWT authorizer does: a token with no `aud` is matched by
/// its `client_id`, which is how Amazon Cognito access tokens name their app.
#[tokio::test]
async fn a_token_without_aud_is_matched_by_its_client_id() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    let mut access = claims_with("aud", Value::Null);
    access["client_id"] = json!("web");
    verifier
        .verify(&token(&rs256("k1"), &access))
        .await
        .expect("client_id names an accepted audience");
    access["client_id"] = json!("another-app");
    assert_eq!(
        refused(&verifier, &token(&rs256("k1"), &access)).await,
        VerifyError::Claims
    );
}

/// When `aud` is present it decides; a matching `client_id` cannot rescue a
/// token issued for someone else, and an `aud` that is not text never matches.
#[tokio::test]
async fn a_present_aud_decides_over_client_id() {
    let jwks = Jwks::serving(vec![jwk("k1")]).await;
    let verifier = jwks.verifier().await;
    let mut other = claims_with("aud", json!("another-app"));
    other["client_id"] = json!("web");
    assert_eq!(
        refused(&verifier, &token(&rs256("k1"), &other)).await,
        VerifyError::Claims
    );
    let numeric = token(&rs256("k1"), &claims_with("aud", json!(7)));
    assert_eq!(refused(&verifier, &numeric).await, VerifyError::Claims);
}
