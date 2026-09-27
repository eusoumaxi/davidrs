//! Changing a delivered message's visibility against an in-process SQS
//! client.
#![cfg(feature = "queue-visibility")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aws_sdk_sqs::config::retry::RetryConfig;
use aws_sdk_sqs::config::{
    AsyncSleep, BehaviorVersion, Credentials, Region, SharedAsyncSleep, Sleep,
};
use aws_sdk_sqs::Client;
use aws_smithy_http_client::test_util::infallible_client_fn;
use davidrs::queue::{Batch, Visibility, MAX_VISIBILITY_TIMEOUT};
use davidrs::RuntimeError;
use serde_json::{json, Value};

const QUEUE_URL: &str = "https://sqs.us-east-1.amazonaws.com/123456789012/orders";

/// The SDK's timer, on Tokio.
#[derive(Debug)]
struct TokioSleep;

impl AsyncSleep for TokioSleep {
    fn sleep(&self, duration: Duration) -> Sleep {
        Sleep::new(tokio::time::sleep(duration))
    }
}

/// The operation and body of each request the fake service received.
type Requests = Arc<Mutex<Vec<(String, Value)>>>;

/// A client that answers every call with this status and body, and the
/// requests it received.
fn sqs(status: u16, answer: Value) -> (Client, Requests) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    let http = infallible_client_fn(move |request| {
        let target = request
            .headers()
            .get("x-amz-target")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body: Value =
            serde_json::from_slice(request.body().bytes().expect("body")).expect("json");
        log.lock().expect("log").push((target, body));
        hyper::Response::builder()
            .status(status)
            .header("content-type", "application/x-amz-json-1.0")
            .body(answer.to_string())
            .expect("response")
    });
    let config = aws_sdk_sqs::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("AKID", "SECRET", None, None, "test"))
        .retry_config(RetryConfig::disabled())
        .sleep_impl(SharedAsyncSleep::new(TokioSleep))
        .http_client(http)
        .build();
    (Client::from_conf(config), requests)
}

fn delivered() -> Visibility {
    let batch: Batch = serde_json::from_value(json!({
        "Records": [{ "messageId": "m-1", "receiptHandle": "handle-1", "body": "" }]
    }))
    .expect("batch");
    Visibility::new(QUEUE_URL, &batch.records[0])
}

#[tokio::test]
async fn a_visibility_change_names_the_queue_the_receipt_handle_and_the_timeout() {
    let (client, requests) = sqs(200, json!({}));
    delivered()
        .set(&client, Duration::from_secs(4))
        .await
        .expect("visibility");
    let requests = requests.lock().expect("log");
    let (target, body) = &requests[0];
    assert_eq!(target, "AmazonSQS.ChangeMessageVisibility");
    assert_eq!(
        *body,
        json!({ "QueueUrl": QUEUE_URL, "ReceiptHandle": "handle-1", "VisibilityTimeout": 4 })
    );
}

#[tokio::test]
async fn a_refused_visibility_change_is_an_error() {
    let (client, _requests) = sqs(
        400,
        json!({ "__type": "com.amazonaws.sqs#ReceiptHandleIsInvalid", "message": "expired" }),
    );
    let failure = delivered()
        .set(&client, Duration::from_secs(4))
        .await
        .expect_err("refused");
    assert_eq!(failure.to_string(), "changing message visibility");
}

/// SQS counts whole seconds; rounding down would bring a message back early.
#[tokio::test]
async fn a_fraction_of_a_second_is_rounded_up() {
    let (client, requests) = sqs(200, json!({}));
    delivered()
        .set(&client, Duration::from_millis(3_500))
        .await
        .expect("visibility");
    assert_eq!(requests.lock().expect("log")[0].1["VisibilityTimeout"], 4);
}

#[tokio::test]
async fn twelve_hours_is_the_longest_delay_and_a_longer_one_never_reaches_sqs() {
    let (client, requests) = sqs(200, json!({}));
    let visibility = delivered();
    visibility
        .set(&client, MAX_VISIBILITY_TIMEOUT)
        .await
        .expect("the maximum");
    let failure = visibility
        .set(&client, MAX_VISIBILITY_TIMEOUT + Duration::from_nanos(1))
        .await
        .expect_err("refused");
    assert!(
        matches!(failure, RuntimeError::Configuration(_)),
        "{failure}"
    );
    let requests = requests.lock().expect("log");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["VisibilityTimeout"], 43_200);
}
