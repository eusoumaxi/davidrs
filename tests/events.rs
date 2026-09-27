//! EventBridge publishing against an in-process SDK client.
//!
//! The fake service sees each `PutEvents` body and answers with per-entry
//! results, so accepted, rejected and unknown outcomes are all exercised.
#![cfg(feature = "events")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aws_sdk_eventbridge::config::retry::RetryConfig;
use aws_sdk_eventbridge::config::{
    AsyncSleep, BehaviorVersion, Credentials, HttpClient, Region, SharedAsyncSleep, Sleep,
};
use aws_sdk_eventbridge::Client;
use aws_smithy_http_client::test_util::{infallible_client_fn, NeverClient};
use davidrs::events::{self, EntryOutcome, EventRoute};
use davidrs::{Deadline, RuntimeError};
use serde_json::{json, Value};

const ORDER_CREATED: EventRoute = EventRoute {
    source: "example.orders",
    detail_type: "order.created",
};

/// The SDK's timer, on Tokio.
#[derive(Debug)]
struct TokioSleep;

impl AsyncSleep for TokioSleep {
    fn sleep(&self, duration: Duration) -> Sleep {
        Sleep::new(tokio::time::sleep(duration))
    }
}

fn client_over(http: impl HttpClient + 'static) -> Client {
    let config = aws_sdk_eventbridge::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("AKID", "SECRET", None, None, "test"))
        .retry_config(RetryConfig::disabled())
        .sleep_impl(SharedAsyncSleep::new(TokioSleep))
        .http_client(http)
        .build();
    Client::from_conf(config)
}

/// A client whose `PutEvents` calls are answered by `respond(body)`, and the
/// bodies it received.
fn eventbridge(
    respond: impl Fn(&Value) -> (u16, Value) + Send + Sync + 'static,
) -> (Client, Arc<Mutex<Vec<Value>>>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&bodies);
    let http = infallible_client_fn(move |request| {
        let body: Value =
            serde_json::from_slice(request.body().bytes().expect("body")).expect("json");
        let (status, answer) = respond(&body);
        log.lock().expect("log").push(body);
        hyper::Response::builder()
            .status(status)
            .header("content-type", "application/x-amz-json-1.1")
            .body(answer.to_string())
            .expect("response")
    });
    (client_over(http), bodies)
}

fn live() -> Deadline {
    Deadline::in_from_now(Duration::from_secs(30))
}

fn order(id: u32) -> Value {
    json!({ "orderId": format!("order-{id}") })
}

#[tokio::test]
async fn every_accepted_entry_reports_its_event_id() {
    let (client, bodies) = eventbridge(|_| {
        (
            200,
            json!({ "FailedEntryCount": 0, "Entries": [{ "EventId": "e-1" }, { "EventId": "e-2" }] }),
        )
    });
    let outcome = events::publish_batch(
        &client,
        "orders-bus",
        &ORDER_CREATED,
        &[order(1), order(2)],
        live(),
    )
    .await
    .expect("publish");
    assert_eq!(
        outcome.entries,
        vec![
            EntryOutcome::Accepted("e-1".to_owned()),
            EntryOutcome::Accepted("e-2".to_owned())
        ]
    );
    assert!(outcome.all_accepted());
    assert!(outcome.failed_indexes().is_empty());
    let entry = &bodies.lock().expect("log")[0]["Entries"][1];
    assert_eq!(entry["EventBusName"], "orders-bus");
    assert_eq!(entry["Source"], "example.orders");
    assert_eq!(entry["DetailType"], "order.created");
}

#[tokio::test]
async fn a_detail_is_sent_as_its_json_text() {
    let (client, bodies) = eventbridge(|_| (200, json!({ "Entries": [{ "EventId": "e-1" }] })));
    let outcome = events::publish(&client, "orders-bus", &ORDER_CREATED, &order(7), live())
        .await
        .expect("publish");
    assert!(outcome.all_accepted());
    assert_eq!(
        bodies.lock().expect("log")[0]["Entries"][0]["Detail"],
        r#"{"orderId":"order-7"}"#
    );
}

#[tokio::test]
async fn a_rejected_entry_reports_its_code_and_message() {
    let (client, _bodies) = eventbridge(|_| {
        (
            200,
            json!({
                "FailedEntryCount": 1,
                "Entries": [
                    { "EventId": "e-1" },
                    { "ErrorCode": "InternalFailure", "ErrorMessage": "try again" },
                ],
            }),
        )
    });
    let outcome = events::publish_batch(
        &client,
        "orders-bus",
        &ORDER_CREATED,
        &[order(1), order(2)],
        live(),
    )
    .await
    .expect("publish");
    assert_eq!(
        outcome.entries[1],
        EntryOutcome::Rejected {
            code: "InternalFailure".to_owned(),
            message: "try again".to_owned(),
        }
    );
    assert!(!outcome.all_accepted());
    assert_eq!(outcome.failed_indexes(), vec![1]);
}

#[tokio::test]
async fn entries_the_service_does_not_account_for_are_unknown() {
    let (client, _bodies) = eventbridge(|_| (200, json!({ "Entries": [{}] })));
    let outcome = events::publish_batch(
        &client,
        "orders-bus",
        &ORDER_CREATED,
        &[order(1), order(2)],
        live(),
    )
    .await
    .expect("publish");
    assert_eq!(
        outcome.entries,
        vec![EntryOutcome::Unknown, EntryOutcome::Unknown]
    );
    assert!(!EntryOutcome::Unknown.is_accepted());
    assert_eq!(outcome.failed_indexes(), vec![0, 1]);
}

#[tokio::test]
async fn a_call_that_misses_its_deadline_leaves_every_entry_unknown() {
    let client = client_over(NeverClient::new());
    let deadline = Deadline::in_from_now(Duration::from_millis(50));
    let outcome = events::publish_batch(
        &client,
        "orders-bus",
        &ORDER_CREATED,
        &[order(1), order(2)],
        deadline,
    )
    .await
    .expect("publish");
    assert_eq!(
        outcome.entries,
        vec![EntryOutcome::Unknown, EntryOutcome::Unknown]
    );
}

#[tokio::test]
async fn a_failed_call_is_an_error_naming_the_bus() {
    let (client, _bodies) = eventbridge(|_| {
        (
            400,
            json!({ "__type": "ResourceNotFoundException", "message": "no such bus" }),
        )
    });
    let failure = events::publish(&client, "orders-bus", &ORDER_CREATED, &order(1), live())
        .await
        .expect_err("fails");
    assert_eq!(failure.to_string(), "publishing to orders-bus");
}

#[tokio::test]
async fn more_entries_than_one_call_takes_are_refused_before_any_call() {
    let (client, bodies) = eventbridge(|_| (200, json!({})));
    let details: Vec<Value> = (0..=events::MAX_ENTRIES as u32).map(order).collect();
    let failure = events::publish_batch(&client, "orders-bus", &ORDER_CREATED, &details, live())
        .await
        .expect_err("too many");
    assert!(matches!(
        failure,
        RuntimeError::LimitExceeded {
            kind: "event entries",
            limit: 10
        }
    ));
    assert!(bodies.lock().expect("log").is_empty());
}

#[tokio::test]
async fn an_empty_batch_makes_no_call() {
    let (client, bodies) = eventbridge(|_| (200, json!({})));
    let outcome =
        events::publish_batch::<Value>(&client, "orders-bus", &ORDER_CREATED, &[], live())
            .await
            .expect("publish");
    assert!(outcome.entries.is_empty());
    assert!(bodies.lock().expect("log").is_empty());
}

#[tokio::test]
async fn a_detail_that_cannot_be_serialized_is_an_error() {
    let (client, bodies) = eventbridge(|_| (200, json!({})));
    let detail = BTreeMap::from([(vec![1_u8], 1)]);
    let failure = events::publish(&client, "orders-bus", &ORDER_CREATED, &detail, live())
        .await
        .expect_err("not JSON");
    assert_eq!(failure.to_string(), "serializing an event detail");
    assert!(bodies.lock().expect("log").is_empty());
}
