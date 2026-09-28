//! Credentials stay valid across an execution-role rotation in a warm Lambda.
//!
//! `sdk_config` does not snapshot the role credentials Lambda injects: its
//! credential provider re-reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`
//! and the optional `AWS_SESSION_TOKEN` on every resolution, and honours
//! `AWS_CREDENTIAL_EXPIRATION` when it is present. These tests rotate the
//! environment between resolutions and confirm a warm `SdkConfig` keeps
//! signing with the current set.
//!
//! Every test here changes process environment variables, so they take one
//! lock and run one at a time. The provider re-reads the environment when
//! `provide_credentials` is called, so [`current`] holds the lock only for
//! that call and releases it before the await; no `std::sync::Mutex` guard
//! spans an await point.
#![cfg(feature = "aws")]

mod support;

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials as _;
use aws_credential_types::provider::SharedCredentialsProvider;
use davidrs::aws::{Trust, sdk_config};

static ENVIRONMENT: Mutex<()> = Mutex::new(());

/// Takes the environment lock and sets the variables Lambda injects for the
/// given access key and secret, with no session token and no expiration.
fn lambda_environment(access_key_id: &str, secret_access_key: &str) -> MutexGuard<'static, ()> {
    let guard = ENVIRONMENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    support::env::set("AWS_ACCESS_KEY_ID", access_key_id);
    support::env::set("AWS_SECRET_ACCESS_KEY", secret_access_key);
    support::env::set("AWS_REGION", "eu-west-1");
    support::env::remove("AWS_SESSION_TOKEN");
    support::env::remove("AWS_CREDENTIAL_EXPIRATION");
    guard
}

/// Takes the environment lock without resetting the variables, for tests that
/// mutate the set Lambda rotates between resolutions.
fn environment_lock() -> MutexGuard<'static, ()> {
    ENVIRONMENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Resolves the current credentials: reads the environment while the lock is
/// held (the provider re-reads it on every call), then drops the lock before
/// the await so no `std::sync::Mutex` guard spans an await point.
async fn current(
    provider: &SharedCredentialsProvider,
    guard: MutexGuard<'static, ()>,
) -> Credentials {
    let future = provider.provide_credentials();
    drop(guard);
    future.await.expect("credentials")
}

/// A warm `SdkConfig` reused across a credential rotation resolves the
/// rotated access key and secret, because the provider re-reads the
/// environment on every resolution; a fresh `SdkConfig` reads it too.
#[tokio::test]
async fn a_warm_config_picks_up_rotated_credentials_after_a_rotation() {
    let _environment = lambda_environment("AKIDCOLD", "secret-cold");
    let provider = sdk_config(Trust::NativeRoots)
        .expect("config")
        .credentials_provider()
        .expect("credentials");

    let cold = current(&provider, _environment).await;
    assert_eq!(cold.access_key_id(), "AKIDCOLD");
    assert_eq!(cold.secret_access_key(), "secret-cold");

    let _environment = environment_lock();
    support::env::set("AWS_ACCESS_KEY_ID", "AKIDROTATED");
    support::env::set("AWS_SECRET_ACCESS_KEY", "secret-rotated");

    let warm = current(&provider, _environment).await;
    assert_eq!(
        warm.access_key_id(),
        "AKIDROTATED",
        "the warm `SdkConfig` re-reads the environment and picks up the rotated key"
    );
    assert_eq!(warm.secret_access_key(), "secret-rotated");

    let _environment = environment_lock();
    let provider = sdk_config(Trust::NativeRoots)
        .expect("config")
        .credentials_provider()
        .expect("credentials");
    let rotated = current(&provider, _environment).await;
    assert_eq!(rotated.access_key_id(), "AKIDROTATED");
    assert_eq!(rotated.secret_access_key(), "secret-rotated");
}

/// A warm `SdkConfig` picks up a rotated `AWS_SESSION_TOKEN` on the next
/// resolution, with the access key held constant.
#[tokio::test]
async fn a_warm_config_picks_up_a_rotated_session_token() {
    let _environment = lambda_environment("AKIDCOLD", "secret-cold");
    support::env::set("AWS_SESSION_TOKEN", "token-cold");
    let provider = sdk_config(Trust::NativeRoots)
        .expect("config")
        .credentials_provider()
        .expect("credentials");

    let cold = current(&provider, _environment).await;
    assert_eq!(cold.session_token(), Some("token-cold"));

    let _environment = environment_lock();
    support::env::set("AWS_SESSION_TOKEN", "token-rotated");

    let warm = current(&provider, _environment).await;
    assert_eq!(
        warm.session_token(),
        Some("token-rotated"),
        "the warm config re-reads the session token on every resolution"
    );
}

/// `AWS_CREDENTIAL_EXPIRATION` is parsed and passed to the SDK, so the lazy
/// cache has a real staleness signal.
#[tokio::test]
async fn the_provider_honours_credential_expiration_when_present() {
    let _environment = lambda_environment("AKIDCOLD", "secret-cold");
    support::env::set("AWS_CREDENTIAL_EXPIRATION", "2024-01-01T00:00:00Z");
    let provider = sdk_config(Trust::NativeRoots)
        .expect("config")
        .credentials_provider()
        .expect("credentials");
    let creds = current(&provider, _environment).await;
    assert_eq!(
        creds.expiry(),
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_704_067_200)),
        "the parsed expiration is passed to the SDK, giving the cache a staleness signal"
    );
}

/// A numeric offset in `AWS_CREDENTIAL_EXPIRATION` is normalised to UTC, so
/// `2024-01-01T02:00:00+02:00` resolves to the same instant as `…00:00:00Z`.
#[tokio::test]
async fn the_provider_normalises_a_utc_offset_in_credential_expiration() {
    let _environment = lambda_environment("AKIDCOLD", "secret-cold");
    support::env::set("AWS_CREDENTIAL_EXPIRATION", "2024-01-01T02:00:00+02:00");
    let provider = sdk_config(Trust::NativeRoots)
        .expect("config")
        .credentials_provider()
        .expect("credentials");
    let creds = current(&provider, _environment).await;
    assert_eq!(
        creds.expiry(),
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_704_067_200)),
        "a numeric offset is normalised to UTC before being passed to the SDK"
    );
}

/// Without `AWS_CREDENTIAL_EXPIRATION` there is no expiry; the provider still
/// re-reads the credential variables on each resolution.
#[tokio::test]
async fn the_provider_has_no_expiry_when_credential_expiration_is_absent() {
    let _environment = lambda_environment("AKIDCOLD", "secret-cold");
    let provider = sdk_config(Trust::NativeRoots)
        .expect("config")
        .credentials_provider()
        .expect("credentials");
    let creds = current(&provider, _environment).await;
    assert!(
        creds.expiry().is_none(),
        "without `AWS_CREDENTIAL_EXPIRATION` there is no expiry; the provider still re-reads"
    );
}

/// A malformed `AWS_CREDENTIAL_EXPIRATION` is dropped rather than failing the
/// resolution: the credentials still resolve, with no expiry set.
#[tokio::test]
async fn an_unparsable_credential_expiration_is_ignored_safely() {
    let _environment = lambda_environment("AKIDCOLD", "secret-cold");
    support::env::set("AWS_CREDENTIAL_EXPIRATION", "not-a-timestamp");
    let provider = sdk_config(Trust::NativeRoots)
        .expect("config")
        .credentials_provider()
        .expect("credentials");
    let creds = current(&provider, _environment).await;
    assert_eq!(creds.access_key_id(), "AKIDCOLD");
    assert!(
        creds.expiry().is_none(),
        "an unparsable expiration is dropped rather than failing the resolution"
    );
}

/// The reserved `AWS_*` variables cannot normally be removed by a function,
/// but the fail-safe path is checked: when the environment no longer holds an
/// access key, the provider surfaces a credential error rather than an empty
/// key.
#[tokio::test]
async fn a_missing_access_key_after_rotation_fails_resolution() {
    let _environment = lambda_environment("AKIDCOLD", "secret-cold");
    let provider = sdk_config(Trust::NativeRoots)
        .expect("config")
        .credentials_provider()
        .expect("credentials");

    support::env::remove("AWS_ACCESS_KEY_ID");
    let future = provider.provide_credentials();
    drop(_environment);
    let failure = future.await;
    assert!(
        failure.is_err(),
        "a missing access key surfaces a credential error, not an empty credential"
    );
}
