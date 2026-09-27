//! SQS partial-batch processing through the public API.
//!
//! The rule: a message is either deleted because it succeeded, or reported so
//! SQS redelivers it. Nothing is ever silently dropped, including messages the
//! invocation ran out of time to attempt.
#![cfg(feature = "queue")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use davidrs::queue::{Batch, BatchResponse, Delivery, Disposition};
use davidrs::{Context, Deadline, Invocation};

const STANDARD: &str = "arn:aws:sqs:us-east-1:123456789012:orders";
const FIFO: &str = "arn:aws:sqs:us-east-1:123456789012:orders.fifo";

/// A batch in the wire shape: `Batch` is `#[non_exhaustive]`, so a crate
/// using this one builds it the way Lambda does, from JSON.
fn batch(queue: &str, bodies: &[(&str, &str)]) -> Batch {
    let records: Vec<serde_json::Value> = bodies
        .iter()
        .map(|(id, body)| {
            serde_json::json!({
                "messageId": id,
                "receiptHandle": format!("handle-{id}"),
                "body": body,
                "attributes": { "ApproximateReceiveCount": "1" },
                "eventSourceARN": queue,
            })
        })
        .collect();
    serde_json::from_value(serde_json::json!({ "Records": records })).expect("batch")
}

fn with_budget(budget: Duration) -> Invocation {
    Invocation::new("request-1", Deadline::in_from_now(budget))
}

fn live() -> Invocation {
    with_budget(Duration::from_secs(30))
}

fn failed(response: &BatchResponse) -> Vec<&str> {
    response
        .batch_item_failures
        .iter()
        .map(|item| item.item_identifier.as_str())
        .collect()
}

/// Answers by body: `ok` deletes, `later` retries, anything else fails.
async fn by_body(
    _app: Arc<()>,
    delivery: Delivery,
    _context: Context<()>,
) -> Result<Disposition, String> {
    match delivery.body.as_str() {
        "ok" => Ok(Disposition::Delete),
        "later" => Ok(Disposition::Retry),
        other => Err(format!("cannot handle {other}")),
    }
}

async fn process(queue: &str, bodies: &[(&str, &str)], invocation: &Invocation) -> BatchResponse {
    davidrs::queue::process(Arc::new(()), batch(queue, bodies), invocation, &by_body).await
}

#[tokio::test]
async fn one_bad_message_does_not_take_the_good_ones_with_it() {
    let response = process(STANDARD, &[("a", "ok"), ("b", "bad"), ("c", "ok")], &live()).await;
    assert_eq!(failed(&response), vec!["b"]);
}

#[tokio::test]
async fn an_explicit_retry_is_reported_and_the_batch_goes_on() {
    let response = process(STANDARD, &[("a", "later"), ("b", "ok")], &live()).await;
    assert_eq!(failed(&response), vec!["a"]);
}

#[tokio::test]
async fn a_fully_successful_batch_reports_nothing_in_the_native_shape() {
    let response = process(STANDARD, &[("a", "ok"), ("b", "ok")], &live()).await;
    assert_eq!(
        serde_json::to_string(&response).expect("json"),
        r#"{"batchItemFailures":[]}"#
    );
}

#[tokio::test]
async fn a_fifo_failure_stops_the_batch_and_reports_every_later_message() {
    let response = process(FIFO, &[("a", "ok"), ("b", "bad"), ("c", "ok")], &live()).await;
    assert_eq!(failed(&response), vec!["b", "c"]);
}

#[tokio::test]
async fn a_fifo_retry_stops_the_batch_too() {
    let response = process(FIFO, &[("a", "later"), ("b", "ok")], &live()).await;
    assert_eq!(failed(&response), vec!["a", "b"]);
}

#[tokio::test]
async fn an_invocation_already_out_of_budget_reports_every_message() {
    let expired = Invocation::new(
        "request-1",
        Deadline::at(Instant::now() - Duration::from_secs(1)),
    );
    let response = process(STANDARD, &[("a", "ok"), ("b", "ok"), ("c", "ok")], &expired).await;
    assert_eq!(failed(&response), vec!["a", "b", "c"]);
}

#[tokio::test]
async fn a_handler_that_overruns_the_deadline_is_reported_with_every_later_message() {
    let started = Instant::now();
    let response = davidrs::queue::process(
        Arc::new(()),
        batch(STANDARD, &[("a", "ok"), ("b", "slow"), ("c", "ok")]),
        &with_budget(Duration::from_millis(300)),
        &|_app, delivery: Delivery, _context| async move {
            if delivery.body == "slow" {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            Ok::<_, String>(Disposition::Delete)
        },
    )
    .await;
    assert_eq!(failed(&response), vec!["b", "c"]);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "answers before the deadline"
    );
}

/// The first handler blocks past the budget but still finishes; the next
/// message finds the budget gone and is reported without being attempted.
#[tokio::test]
async fn a_budget_that_runs_out_between_messages_reports_the_rest_unattempted() {
    let attempted = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&attempted);
    let response = davidrs::queue::process(
        Arc::new(()),
        batch(STANDARD, &[("a", "ok"), ("b", "ok"), ("c", "ok")]),
        &with_budget(Duration::from_millis(150)),
        &move |_app, delivery: Delivery, _context| {
            let log = Arc::clone(&log);
            async move {
                log.lock().expect("log").push(delivery.message_id);
                std::thread::sleep(Duration::from_millis(100));
                Ok::<_, String>(Disposition::Delete)
            }
        },
    )
    .await;
    assert_eq!(*attempted.lock().expect("log"), vec!["a"]);
    assert_eq!(failed(&response), vec!["b", "c"]);
}

#[test]
fn the_native_envelope_deserializes_including_the_arn_spelling() {
    let raw = serde_json::json!({
        "Records": [{
            "messageId": "m-1",
            "receiptHandle": "handle-1",
            "body": "{\"page\":1}",
            "attributes": { "ApproximateReceiveCount": "3" },
            "eventSourceARN": STANDARD
        }]
    });
    let batch: Batch = serde_json::from_value(raw).expect("envelope");
    let record = &batch.records[0];
    assert_eq!(record.message_id, "m-1");
    assert_eq!(record.receipt_handle, "handle-1");
    assert_eq!(record.receive_count(), 3);
    assert_eq!(record.event_source_arn.as_deref(), Some(STANDARD));
    assert_eq!(
        record.json::<HashMap<String, u32>>().expect("body")["page"],
        1
    );
}

#[test]
fn an_absent_or_unparseable_receive_count_reads_as_a_first_attempt() {
    let raw = serde_json::json!({
        "Records": [
            { "messageId": "a", "receiptHandle": "handle-a", "body": "" },
            {
                "messageId": "b",
                "receiptHandle": "handle-b",
                "body": "",
                "attributes": { "ApproximateReceiveCount": "many" }
            }
        ]
    });
    let batch: Batch = serde_json::from_value(raw).expect("envelope");
    assert_eq!(batch.records[0].receive_count(), 1);
    assert_eq!(batch.records[1].receive_count(), 1);
    assert_eq!(batch.records[0].event_source_arn, None);
}

#[test]
fn a_body_of_the_wrong_shape_is_an_error_naming_the_message() {
    let batch = batch(STANDARD, &[("m-1", "not json")]);
    let failure = batch.records[0]
        .json::<serde_json::Value>()
        .expect_err("not JSON");
    assert_eq!(failure.to_string(), "decoding message m-1");
}

#[test]
fn a_reported_message_uses_the_native_item_shape() {
    let mut response = BatchResponse::default();
    response.fail("m-1");
    assert_eq!(
        serde_json::to_value(&response).expect("json"),
        serde_json::json!({ "batchItemFailures": [{ "itemIdentifier": "m-1" }] })
    );
}
