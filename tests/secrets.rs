//! Secrets Manager reads against an in-process SDK client.
//!
//! The rule under test: a secret's name may appear in an error, its value
//! never does.
#![cfg(feature = "secrets")]

use std::time::Duration;

use aws_sdk_secretsmanager::config::retry::RetryConfig;
use aws_sdk_secretsmanager::config::{
    AsyncSleep, BehaviorVersion, Credentials, Region, SharedAsyncSleep, Sleep,
};
use aws_sdk_secretsmanager::Client;
use aws_smithy_http_client::test_util::infallible_client_fn;
use davidrs::{error_chain, RuntimeError};
use serde_json::{json, Value};

/// The SDK's timer, on Tokio.
#[derive(Debug)]
struct TokioSleep;

impl AsyncSleep for TokioSleep {
    fn sleep(&self, duration: Duration) -> Sleep {
        Sleep::new(tokio::time::sleep(duration))
    }
}

/// A client whose `GetSecretValue` calls are answered with this status and
/// body, after checking the request names `example/upstream`.
fn secrets(status: u16, answer: Value) -> Client {
    let http = infallible_client_fn(move |request| {
        let body: Value =
            serde_json::from_slice(request.body().bytes().expect("body")).expect("json");
        assert_eq!(body["SecretId"], "example/upstream");
        hyper::Response::builder()
            .status(status)
            .header("content-type", "application/x-amz-json-1.1")
            .body(answer.to_string())
            .expect("response")
    });
    let config = aws_sdk_secretsmanager::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("AKID", "SECRET", None, None, "test"))
        .retry_config(RetryConfig::disabled())
        .sleep_impl(SharedAsyncSleep::new(TokioSleep))
        .http_client(http)
        .build();
    Client::from_conf(config)
}

fn stored(value: &str) -> Client {
    secrets(
        200,
        json!({ "Name": "example/upstream", "SecretString": value }),
    )
}

#[derive(Debug, serde::Deserialize)]
struct Upstream {
    api_key: String,
}

#[tokio::test]
async fn a_string_secret_is_returned_as_stored() {
    let value = davidrs::secrets::string(&stored("s3cr3t"), "example/upstream")
        .await
        .expect("secret");
    assert_eq!(value, "s3cr3t");
}

#[tokio::test]
async fn a_json_secret_deserializes_into_a_type() {
    let client = stored(r#"{"api_key":"s3cr3t"}"#);
    let upstream: Upstream = davidrs::secrets::json(&client, "example/upstream")
        .await
        .expect("secret");
    assert_eq!(upstream.api_key, "s3cr3t");
}

#[tokio::test]
async fn a_missing_secret_is_an_error_naming_it() {
    let client = secrets(
        400,
        json!({ "__type": "ResourceNotFoundException", "Message": "Secrets Manager can't find the specified secret." }),
    );
    let failure = davidrs::secrets::string(&client, "example/upstream")
        .await
        .expect_err("missing");
    assert_eq!(failure.to_string(), "reading secret example/upstream");
}

#[tokio::test]
async fn a_binary_only_secret_has_no_string_value() {
    let client = secrets(
        200,
        json!({ "Name": "example/upstream", "SecretBinary": "AQI=" }),
    );
    let failure = davidrs::secrets::string(&client, "example/upstream")
        .await
        .expect_err("binary");
    assert!(matches!(failure, RuntimeError::Configuration(_)));
    assert_eq!(
        failure.to_string(),
        "configuration: secret example/upstream has no string value"
    );
}

#[tokio::test]
async fn a_secret_of_the_wrong_shape_is_reported_without_its_value() {
    let client = stored(r#"{"api_key": 12345678}"#);
    let failure = davidrs::secrets::json::<Upstream>(&client, "example/upstream")
        .await
        .expect_err("wrong shape");
    let text = error_chain(&failure);
    assert!(text.contains("example/upstream"), "{text}");
    assert!(text.contains("line 1, column"), "{text}");
    assert!(!text.contains("12345678"), "{text}");
}
