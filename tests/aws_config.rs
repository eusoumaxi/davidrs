//! SDK configuration from the Lambda environment.
//!
//! Every test here changes process environment variables, so they take one
//! lock and run one at a time. The credentials are read when the SDK resolves
//! them, so a test holds the lock until it has resolved them.
#![cfg(feature = "aws")]

mod support;

use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials as _;
use aws_types::SdkConfig;
use davidrs::RuntimeError;
use davidrs::aws::{Trust, sdk_config};
use tokio::sync::{Mutex, MutexGuard};

static ENVIRONMENT: Mutex<()> = Mutex::const_new(());

/// Takes the environment lock and sets the variables Lambda injects, with a
/// session token or without one.
async fn lambda_environment(session_token: Option<&str>) -> MutexGuard<'static, ()> {
    let guard = ENVIRONMENT.lock().await;
    support::env::set("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
    support::env::set("AWS_SECRET_ACCESS_KEY", "secret-example");
    support::env::set("AWS_REGION", "eu-west-1");
    match session_token {
        Some(token) => support::env::set("AWS_SESSION_TOKEN", token),
        None => support::env::remove("AWS_SESSION_TOKEN"),
    }
    guard
}

/// The credentials the configuration resolves now.
async fn resolve(config: &SdkConfig) -> Credentials {
    config
        .credentials_provider()
        .expect("credentials")
        .provide_credentials()
        .await
        .expect("credentials")
}

#[tokio::test]
async fn the_configuration_takes_credentials_and_region_from_the_environment() {
    let _environment = lambda_environment(Some("token-example")).await;
    let config = sdk_config(Trust::NativeRoots).expect("config");
    assert_eq!(
        config.region().map(ToString::to_string).as_deref(),
        Some("eu-west-1")
    );
    assert!(config.http_client().is_some());
    assert!(config.behavior_version().is_some());
    let credentials = resolve(&config).await;
    assert_eq!(credentials.access_key_id(), "AKIDEXAMPLE");
    assert_eq!(credentials.secret_access_key(), "secret-example");
    assert_eq!(credentials.session_token(), Some("token-example"));
}

#[tokio::test]
async fn a_session_token_is_optional() {
    let _environment = lambda_environment(None).await;
    let config = sdk_config(Trust::NativeRoots).expect("config");
    assert_eq!(resolve(&config).await.session_token(), None);
}

/// Credentials that change while the function is warm are used from the
/// next resolution on, not the ones read when the configuration was built.
#[tokio::test]
async fn the_credentials_are_read_again_at_each_resolution() {
    let _environment = lambda_environment(Some("token-example")).await;
    let config = sdk_config(Trust::NativeRoots).expect("config");
    support::env::set("AWS_ACCESS_KEY_ID", "AKIDROTATED");
    support::env::set("AWS_SESSION_TOKEN", "token-rotated");
    let credentials = resolve(&config).await;
    assert_eq!(credentials.access_key_id(), "AKIDROTATED");
    assert_eq!(credentials.session_token(), Some("token-rotated"));
}

#[tokio::test]
async fn a_credential_removed_after_startup_fails_the_resolution_naming_it() {
    let _environment = lambda_environment(None).await;
    let config = sdk_config(Trust::NativeRoots).expect("config");
    support::env::remove("AWS_SECRET_ACCESS_KEY");
    let error = config
        .credentials_provider()
        .expect("credentials")
        .provide_credentials()
        .await
        .expect_err("missing");
    assert_eq!(
        std::error::Error::source(&error).map(ToString::to_string),
        Some("missing environment variable AWS_SECRET_ACCESS_KEY".to_owned())
    );
}

#[tokio::test]
async fn a_pinned_bundle_is_accepted_without_any_io() {
    let _environment = lambda_environment(None).await;
    let bundle = b"-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n";
    let config = sdk_config(Trust::Pem(bundle)).expect("config");
    assert!(config.http_client().is_some());
}

#[tokio::test]
async fn a_missing_variable_is_a_configuration_error_naming_it() {
    let _environment = lambda_environment(None).await;
    for name in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"] {
        let value = std::env::var(name).expect("set");
        support::env::remove(name);
        let failure = sdk_config(Trust::NativeRoots).expect_err("missing");
        support::env::set(name, value);
        assert!(matches!(failure, RuntimeError::Configuration(_)));
        assert_eq!(
            failure.to_string(),
            format!("configuration: missing environment variable {name}")
        );
    }
}

#[tokio::test]
async fn the_configuration_carries_the_timer_sdk_retries_need() {
    let _environment = lambda_environment(None).await;
    let config = sdk_config(Trust::NativeRoots).expect("config");
    assert!(config.sleep_impl().is_some());
}

/// A client built from the configuration alone — no timer supplied by the
/// test — can be created and can call the service.
#[cfg(feature = "dynamo")]
#[tokio::test]
async fn a_service_client_built_from_the_configuration_works() {
    use aws_smithy_http_client::test_util::infallible_client_fn;

    let _environment = lambda_environment(None).await;
    let config = sdk_config(Trust::NativeRoots).expect("config");
    let http = infallible_client_fn(|_request| {
        hyper::Response::builder()
            .status(200)
            .header("content-type", "application/x-amz-json-1.0")
            .body(r#"{"TableNames":[]}"#)
            .expect("response")
    });
    let client = aws_sdk_dynamodb::Client::from_conf(
        aws_sdk_dynamodb::config::Builder::from(&config)
            .http_client(http)
            .build(),
    );
    let output = client
        .list_tables()
        .send()
        .await
        .expect("the call succeeds");
    assert!(output.table_names().is_empty());
}
