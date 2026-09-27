//! SDK configuration from the Lambda environment.
//!
//! Every test here changes process environment variables, so they take one
//! lock and run one at a time.
#![cfg(feature = "aws")]

use std::sync::{Mutex, MutexGuard};

use aws_credential_types::provider::ProvideCredentials as _;
use davidrs::aws::{sdk_config, Trust};
use davidrs::RuntimeError;

static ENVIRONMENT: Mutex<()> = Mutex::new(());

/// Takes the environment lock and sets the variables Lambda injects, with a
/// session token or without one.
fn lambda_environment(session_token: Option<&str>) -> MutexGuard<'static, ()> {
    let guard = ENVIRONMENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-example");
    std::env::set_var("AWS_REGION", "eu-west-1");
    match session_token {
        Some(token) => std::env::set_var("AWS_SESSION_TOKEN", token),
        None => std::env::remove_var("AWS_SESSION_TOKEN"),
    }
    guard
}

#[tokio::test]
async fn the_configuration_takes_credentials_and_region_from_the_environment() {
    let config = {
        let _environment = lambda_environment(Some("token-example"));
        sdk_config(Trust::NativeRoots).expect("config")
    };
    assert_eq!(
        config.region().map(ToString::to_string).as_deref(),
        Some("eu-west-1")
    );
    assert!(config.http_client().is_some());
    assert!(config.behavior_version().is_some());
    let credentials = config
        .credentials_provider()
        .expect("credentials")
        .provide_credentials()
        .await
        .expect("credentials");
    assert_eq!(credentials.access_key_id(), "AKIDEXAMPLE");
    assert_eq!(credentials.secret_access_key(), "secret-example");
    assert_eq!(credentials.session_token(), Some("token-example"));
}

#[tokio::test]
async fn a_session_token_is_optional() {
    let config = {
        let _environment = lambda_environment(None);
        sdk_config(Trust::NativeRoots).expect("config")
    };
    let credentials = config
        .credentials_provider()
        .expect("credentials")
        .provide_credentials()
        .await
        .expect("credentials");
    assert_eq!(credentials.session_token(), None);
}

#[test]
fn a_pinned_bundle_is_accepted_without_any_io() {
    let _environment = lambda_environment(None);
    let bundle = b"-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n";
    let config = sdk_config(Trust::Pem(bundle)).expect("config");
    assert!(config.http_client().is_some());
}

#[test]
fn a_missing_variable_is_a_configuration_error_naming_it() {
    let _environment = lambda_environment(None);
    for name in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"] {
        let value = std::env::var(name).expect("set");
        std::env::remove_var(name);
        let failure = sdk_config(Trust::NativeRoots).expect_err("missing");
        std::env::set_var(name, value);
        assert!(matches!(failure, RuntimeError::Configuration(_)));
        assert_eq!(
            failure.to_string(),
            format!("configuration: missing environment variable {name}")
        );
    }
}

#[tokio::test]
async fn the_configuration_carries_the_timer_sdk_retries_need() {
    let config = {
        let _environment = lambda_environment(None);
        sdk_config(Trust::NativeRoots).expect("config")
    };
    assert!(config.sleep_impl().is_some());
}

/// A client built from the configuration alone — no timer supplied by the
/// test — can be created and can call the service.
#[cfg(feature = "dynamo")]
#[tokio::test]
async fn a_service_client_built_from_the_configuration_works() {
    use aws_smithy_http_client::test_util::infallible_client_fn;

    let config = {
        let _environment = lambda_environment(None);
        sdk_config(Trust::NativeRoots).expect("config")
    };
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
