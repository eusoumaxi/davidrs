//! DynamoDB helpers against an in-process SDK client.
//!
//! Every call is answered by a closure that sees the operation name and the
//! request body, so partial outcomes, retries and limits are exercised
//! offline and deterministically.
#![cfg(feature = "dynamo")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aws_sdk_dynamodb::config::retry::RetryConfig;
use aws_sdk_dynamodb::config::{
    AsyncSleep, BehaviorVersion, Credentials, Region, SharedAsyncSleep, Sleep,
};
use aws_sdk_dynamodb::types::{AttributeValue, PutRequest, WriteRequest};
use aws_sdk_dynamodb::Client;
use aws_smithy_http_client::test_util::infallible_client_fn;
use davidrs::table::{self, Item, PageLimits, Stop};
use davidrs::Deadline;
use serde_json::{json, Value};

/// One request the fake service received.
#[derive(Debug, Clone)]
struct Call {
    operation: String,
    body: Value,
    at: Instant,
}

type Calls = Arc<Mutex<Vec<Call>>>;

/// The SDK's timer, on Tokio.
#[derive(Debug)]
struct TokioSleep;

impl AsyncSleep for TokioSleep {
    fn sleep(&self, duration: Duration) -> Sleep {
        Sleep::new(tokio::time::sleep(duration))
    }
}

/// A client whose calls are answered by `respond(operation, body)` with a
/// status and a JSON body, and the log of those calls.
fn dynamodb(
    respond: impl Fn(&str, &Value) -> (u16, Value) + Send + Sync + 'static,
) -> (Client, Calls) {
    let calls: Calls = Arc::default();
    let log = Arc::clone(&calls);
    let http = infallible_client_fn(move |request| {
        let operation = request
            .headers()
            .get("x-amz-target")
            .and_then(|value| value.to_str().ok())
            .and_then(|target| target.split('.').nth(1))
            .unwrap_or_default()
            .to_owned();
        let body: Value =
            serde_json::from_slice(request.body().bytes().unwrap_or(b"{}")).expect("body");
        let (status, answer) = respond(&operation, &body);
        log.lock().expect("log").push(Call {
            operation,
            body,
            at: Instant::now(),
        });
        hyper::Response::builder()
            .status(status)
            .header("content-type", "application/x-amz-json-1.0")
            .body(answer.to_string())
            .expect("response")
    });
    let config = aws_sdk_dynamodb::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("AKID", "SECRET", None, None, "test"))
        .retry_config(RetryConfig::disabled())
        .sleep_impl(SharedAsyncSleep::new(TokioSleep))
        .http_client(http)
        .build();
    (Client::from_conf(config), calls)
}

fn calls(log: &Calls) -> Vec<Call> {
    log.lock().expect("log").clone()
}

fn error(code: &str) -> (u16, Value) {
    (
        400,
        json!({ "__type": format!("com.amazonaws.dynamodb.v20120810#{code}"), "message": "refused" }),
    )
}

fn live() -> Deadline {
    Deadline::in_from_now(Duration::from_secs(30))
}

fn expired() -> Deadline {
    Deadline::at(Instant::now() - Duration::from_secs(1))
}

fn key(id: usize) -> Item {
    Item::from([("id".to_owned(), AttributeValue::S(format!("order-{id}")))])
}

fn put(id: usize) -> WriteRequest {
    WriteRequest::builder()
        .put_request(
            PutRequest::builder()
                .set_item(Some(key(id)))
                .build()
                .expect("put"),
        )
        .build()
}

/// A key in the wire shape, as a service response carries it.
fn wire_key(id: usize) -> Value {
    json!({ "id": { "S": format!("order-{id}") } })
}

fn wire_items(ids: impl IntoIterator<Item = usize>) -> Value {
    Value::Array(ids.into_iter().map(wire_key).collect())
}

fn sent_writes(call: &Call) -> Vec<Value> {
    call.body["RequestItems"]["orders"]
        .as_array()
        .expect("writes")
        .clone()
}

fn sent_keys(call: &Call) -> Vec<Value> {
    call.body["RequestItems"]["orders"]["Keys"]
        .as_array()
        .expect("keys")
        .clone()
}

#[tokio::test]
async fn a_batch_write_splits_requests_into_batches_of_twenty_five() {
    let (client, log) = dynamodb(|_, _| (200, json!({})));
    let requests = (0..60).map(put).collect();
    let outcome = table::batch_write(&client, "orders", requests, 5, live())
        .await
        .expect("write");
    let sizes: Vec<usize> = calls(&log)
        .iter()
        .map(|call| sent_writes(call).len())
        .collect();
    assert_eq!(sizes, vec![25, 25, 10]);
    assert_eq!(table::MAX_BATCH_WRITE, 25);
    assert_eq!((outcome.written, outcome.attempts), (60, 3));
    assert!(outcome.is_complete());
}

#[tokio::test]
async fn a_batch_write_sends_unprocessed_requests_again_after_a_pause() {
    let (client, log) = dynamodb(|_, body| {
        let writes = body["RequestItems"]["orders"].as_array().expect("writes");
        if writes.len() == 3 {
            (
                200,
                json!({ "UnprocessedItems": { "orders": [writes[2]] } }),
            )
        } else {
            (200, json!({}))
        }
    });
    let requests = (0..3).map(put).collect();
    let outcome = table::batch_write(&client, "orders", requests, 3, live())
        .await
        .expect("write");
    let calls = calls(&log);
    assert_eq!(
        sent_writes(&calls[1]),
        vec![sent_writes(&calls[0])[2].clone()]
    );
    assert!(
        calls[1].at - calls[0].at >= Duration::from_millis(20),
        "backs off before a retry"
    );
    assert_eq!((outcome.written, outcome.attempts), (3, 2));
    assert!(outcome.is_complete());
}

#[tokio::test]
async fn a_batch_write_out_of_attempts_reports_the_returned_and_the_unsent_requests() {
    let (client, _log) = dynamodb(|_, body| {
        let writes = body["RequestItems"]["orders"].as_array().expect("writes");
        (
            200,
            json!({ "UnprocessedItems": { "orders": writes[20..] } }),
        )
    });
    let requests = (0..30).map(put).collect();
    let outcome = table::batch_write(&client, "orders", requests, 1, live())
        .await
        .expect("write");
    assert_eq!((outcome.written, outcome.attempts), (20, 1));
    assert_eq!(outcome.unprocessed.len(), 10);
    assert_eq!(
        outcome.unprocessed[0],
        put(20),
        "returned requests come first"
    );
    assert_eq!(outcome.unprocessed[9], put(29));
    assert!(!outcome.is_complete());
}

#[tokio::test]
async fn a_batch_write_past_its_deadline_reports_every_request_unsent() {
    let (client, log) = dynamodb(|_, _| (200, json!({})));
    let requests = (0..3).map(put).collect();
    let outcome = table::batch_write(&client, "orders", requests, 3, expired())
        .await
        .expect("write");
    assert!(calls(&log).is_empty());
    assert_eq!(
        (outcome.written, outcome.attempts, outcome.unprocessed.len()),
        (0, 0, 3)
    );
}

#[tokio::test]
async fn a_failed_batch_write_is_an_error_naming_the_table() {
    let (client, _log) = dynamodb(|_, _| error("ResourceNotFoundException"));
    let failure = table::batch_write(&client, "orders", vec![put(1)], 3, live())
        .await
        .expect_err("fails");
    assert_eq!(failure.to_string(), "batch write to orders");
}

#[tokio::test]
async fn a_batch_get_splits_keys_into_batches_of_one_hundred_with_the_configuration() {
    let (client, log) = dynamodb(|_, body| {
        let keys = body["RequestItems"]["orders"]["Keys"].clone();
        (200, json!({ "Responses": { "orders": keys } }))
    });
    let keys = (0..150).map(key).collect();
    let read = table::batch_get(&client, "orders", keys, 3, live(), |batch| {
        batch.projection_expression("id")
    })
    .await
    .expect("read");
    let calls = calls(&log);
    let sizes: Vec<usize> = calls.iter().map(|call| sent_keys(call).len()).collect();
    assert_eq!(sizes, vec![100, 50]);
    assert_eq!(table::MAX_BATCH_GET, 100);
    assert_eq!(
        calls[0].body["RequestItems"]["orders"]["ProjectionExpression"],
        "id"
    );
    assert_eq!(read.items.len(), 150);
    assert_eq!(read.items[149], key(149));
    assert!(read.is_complete());
}

#[tokio::test]
async fn a_batch_get_asks_again_for_unprocessed_keys_only_after_a_pause() {
    let (client, log) = dynamodb(|_, body| {
        let keys = body["RequestItems"]["orders"]["Keys"]
            .as_array()
            .expect("keys");
        if keys.len() == 3 {
            (
                200,
                json!({
                    "Responses": { "orders": keys[..2] },
                    "UnprocessedKeys": { "orders": { "Keys": [keys[2]] } },
                }),
            )
        } else {
            (200, json!({ "Responses": { "orders": keys } }))
        }
    });
    let keys = (0..3).map(key).collect();
    let read = table::batch_get(&client, "orders", keys, 3, live(), |batch| batch)
        .await
        .expect("read");
    let calls = calls(&log);
    assert_eq!(sent_keys(&calls[1]), vec![wire_key(2)]);
    assert!(
        calls[1].at - calls[0].at >= Duration::from_millis(20),
        "backs off before a retry"
    );
    assert_eq!(read.items, (0..3).map(key).collect::<Vec<_>>());
    assert!(read.is_complete());
}

#[tokio::test]
async fn a_batch_get_out_of_attempts_reports_the_batch_and_every_later_one() {
    let (client, log) = dynamodb(|_, body| {
        let keys = body["RequestItems"]["orders"]["Keys"]
            .as_array()
            .expect("keys");
        (
            200,
            json!({
                "Responses": { "orders": keys[10..] },
                "UnprocessedKeys": { "orders": { "Keys": keys[..10] } },
            }),
        )
    });
    let keys = (0..150).map(key).collect();
    let read = table::batch_get(&client, "orders", keys, 1, live(), |batch| batch)
        .await
        .expect("read");
    assert_eq!(calls(&log).len(), 1);
    assert_eq!(read.items.len(), 90);
    assert_eq!(read.unprocessed.len(), 60);
    assert_eq!(read.unprocessed[0], key(0));
    assert_eq!(read.unprocessed[10], key(100));
    assert!(!read.is_complete());
}

#[tokio::test]
async fn a_batch_get_past_its_deadline_reports_every_key() {
    let (client, log) = dynamodb(|_, _| (200, json!({})));
    let keys = (0..3).map(key).collect();
    let read = table::batch_get(&client, "orders", keys, 3, expired(), |batch| batch)
        .await
        .expect("read");
    assert!(calls(&log).is_empty());
    assert_eq!(read.unprocessed, (0..3).map(key).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_failed_batch_get_is_an_error_naming_the_table() {
    let (client, _log) = dynamodb(|_, _| error("ResourceNotFoundException"));
    let failure = table::batch_get(&client, "orders", vec![key(1)], 3, live(), |batch| batch)
        .await
        .expect_err("fails");
    assert_eq!(failure.to_string(), "batch read from orders");
}

#[tokio::test]
async fn a_batch_get_whose_configuration_drops_the_keys_is_an_error() {
    let (client, log) = dynamodb(|_, _| (200, json!({})));
    let failure = table::batch_get(&client, "orders", vec![key(1)], 3, live(), |batch| {
        batch.set_keys(None)
    })
    .await
    .expect_err("fails");
    assert_eq!(failure.to_string(), "building a batch read");
    assert!(calls(&log).is_empty());
}

/// Answers a query or scan with `per_page` items per page from `total`, and
/// a `LastEvaluatedKey` while more remain, as the service does with `Limit`.
fn paged(total: usize, per_page: usize) -> impl Fn(&str, &Value) -> (u16, Value) + Send + Sync {
    move |_, body| {
        let start = body["ExclusiveStartKey"]["id"]["S"]
            .as_str()
            .and_then(|id| id.strip_prefix("order-"))
            .map_or(0, |id| id.parse::<usize>().expect("id") + 1);
        let limit = body["Limit"]
            .as_u64()
            .map_or(per_page, |limit| limit as usize);
        let end = total.min(start + per_page.min(limit));
        let mut answer = json!({ "Items": wire_items(start..end) });
        if end < total {
            answer["LastEvaluatedKey"] = wire_key(end - 1);
        }
        (200, answer)
    }
}

fn query(
    client: &Client,
) -> impl FnMut(Option<Item>) -> aws_sdk_dynamodb::operation::query::builders::QueryFluentBuilder + '_
{
    |start| {
        client
            .query()
            .table_name("orders")
            .key_condition_expression("pk = :pk")
            .expression_attribute_values(":pk", AttributeValue::S("user-1".to_owned()))
            .set_exclusive_start_key(start)
    }
}

#[tokio::test]
async fn a_query_reads_every_page_until_the_service_has_no_more() {
    let (client, log) = dynamodb(paged(5, 2));
    let page = table::query_bounded(PageLimits::new(10, 5), live(), query(&client))
        .await
        .expect("query");
    assert_eq!(page.items, (0..5).map(key).collect::<Vec<_>>());
    assert_eq!((page.next, page.stopped_by), (None, None));
    let calls = calls(&log);
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].operation, "Query");
    assert_eq!(calls[1].body["ExclusiveStartKey"], wire_key(1));
    assert_eq!(
        calls[2].body["Limit"], 6,
        "asks only for the items still wanted"
    );
}

#[tokio::test]
async fn a_query_stops_at_the_item_cap_with_an_exact_key_to_resume() {
    let (client, log) = dynamodb(paged(100, 50));
    let page = table::query_bounded(PageLimits::new(3, 5), live(), query(&client))
        .await
        .expect("query");
    assert_eq!(page.items, (0..3).map(key).collect::<Vec<_>>());
    assert_eq!(page.next, Some(key(2)));
    assert_eq!(page.stopped_by, Some(Stop::Items));
    assert!(!page.is_complete());
    assert_eq!(calls(&log)[0].body["Limit"], 3);
}

#[tokio::test]
async fn a_query_keeps_a_limit_smaller_than_the_cap() {
    let (client, log) = dynamodb(paged(100, 50));
    let mut build = query(&client);
    let page = table::query_bounded(PageLimits::new(10, 1), live(), |start| {
        build(start).limit(4)
    })
    .await
    .expect("query");
    assert_eq!(calls(&log)[0].body["Limit"], 4);
    assert_eq!(page.stopped_by, Some(Stop::Pages));
}

#[tokio::test]
async fn a_page_larger_than_the_cap_is_cut_and_reported_incomplete() {
    let (client, _log) = dynamodb(|_, _| (200, json!({ "Items": wire_items(0..5) })));
    let page = table::query_bounded(PageLimits::new(3, 5), live(), query(&client))
        .await
        .expect("query");
    assert_eq!(page.items.len(), 3);
    assert_eq!((&page.next, page.stopped_by), (&None, Some(Stop::Items)));
    assert!(!page.is_complete());
}

#[tokio::test]
async fn a_query_stops_at_the_page_cap_with_a_key_to_resume() {
    let (client, log) = dynamodb(paged(100, 1));
    let page = table::query_bounded(PageLimits::new(10, 2), live(), query(&client))
        .await
        .expect("query");
    assert_eq!(calls(&log).len(), 2);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.next, Some(key(1)));
    assert_eq!(page.stopped_by, Some(Stop::Pages));
}

#[tokio::test]
async fn a_query_past_its_deadline_reads_nothing_and_is_incomplete() {
    let (client, log) = dynamodb(paged(5, 2));
    let page = table::query_bounded(PageLimits::new(10, 5), expired(), query(&client))
        .await
        .expect("query");
    assert!(calls(&log).is_empty());
    assert!(page.items.is_empty());
    assert_eq!((&page.next, page.stopped_by), (&None, Some(Stop::Deadline)));
    assert!(!page.is_complete());
}

/// The first page blocks until the deadline has passed, so the read stops
/// before the second page with the key to resume from.
#[tokio::test]
async fn a_query_that_reaches_its_deadline_between_pages_keeps_the_key_to_resume() {
    let respond = paged(5, 2);
    let (client, log) = dynamodb(move |operation, body| {
        std::thread::sleep(Duration::from_millis(60));
        respond(operation, body)
    });
    let deadline = Deadline::in_from_now(Duration::from_millis(30));
    let page = table::query_bounded(PageLimits::new(10, 5), deadline, query(&client))
        .await
        .expect("query");
    assert_eq!(calls(&log).len(), 1);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.next, Some(key(1)));
    assert_eq!(page.stopped_by, Some(Stop::Deadline));
}

#[tokio::test]
async fn a_failed_query_page_is_an_error_naming_the_page() {
    let (client, _log) = dynamodb(|_, body| {
        if body.get("ExclusiveStartKey").is_some() {
            error("ProvisionedThroughputExceededException")
        } else {
            (
                200,
                json!({ "Items": wire_items(0..2), "LastEvaluatedKey": wire_key(1) }),
            )
        }
    });
    let failure = table::query_bounded(PageLimits::new(10, 5), live(), query(&client))
        .await
        .expect_err("fails");
    assert_eq!(failure.to_string(), "query page 1");
}

#[tokio::test]
async fn a_scan_pages_under_the_same_limits_as_a_query() {
    let (client, log) = dynamodb(paged(3, 2));
    let page = table::scan_bounded(PageLimits::new(10, 5), live(), |start| {
        client
            .scan()
            .table_name("orders")
            .set_exclusive_start_key(start)
    })
    .await
    .expect("scan");
    assert_eq!(page.items, (0..3).map(key).collect::<Vec<_>>());
    assert!(page.is_complete());
    let calls = calls(&log);
    assert_eq!(calls[0].operation, "Scan");
    assert_eq!(calls[0].body["Limit"], 10);
    assert_eq!(calls[1].body["Limit"], 8);
}

#[tokio::test]
async fn a_failed_scan_page_is_an_error_naming_the_page() {
    let (client, _log) = dynamodb(|_, _| error("ResourceNotFoundException"));
    let failure = table::scan_bounded(PageLimits::new(10, 5), live(), |start| {
        client
            .scan()
            .table_name("orders")
            .set_exclusive_start_key(start)
    })
    .await
    .expect_err("fails");
    assert_eq!(failure.to_string(), "scan page 0");
}

#[test]
fn a_cursor_spells_one_key_one_way_and_round_trips() {
    let key: Item = [("sk", "b"), ("pk", "a")]
        .map(|(name, value)| (name.to_owned(), AttributeValue::S(value.to_owned())))
        .into_iter()
        .collect();
    let token = table::encode_cursor(key.clone()).expect("token");
    assert_eq!(
        token, "eyJwayI6ImEiLCJzayI6ImIifQ==",
        "attributes in name order"
    );
    assert_eq!(table::decode_cursor(&token), Some(key));
}

#[test]
fn a_token_this_module_did_not_write_starts_over() {
    assert_eq!(table::decode_cursor("not base64!"), None);
    assert_eq!(table::decode_cursor("bm90IGpzb24="), None, "not JSON");
    assert_eq!(
        table::decode_cursor("bnVsbA=="),
        None,
        "`null` is not a key"
    );
    assert_eq!(table::decode_cursor("WzFd"), None, "`[1]` is not a key");
}

#[test]
fn a_key_with_a_binary_attribute_has_no_cursor() {
    let key = Item::from([("id".to_owned(), AttributeValue::B(vec![1, 2].into()))]);
    let failure = table::encode_cursor(key).expect_err("binary");
    assert_eq!(failure.to_string(), "encoding a page token");
}

#[test]
fn an_object_drops_only_the_named_attributes() {
    let mut item = Item::from([
        ("PK".to_owned(), AttributeValue::S("user-1".to_owned())),
        ("SK".to_owned(), AttributeValue::S("order-1".to_owned())),
        ("status".to_owned(), AttributeValue::S("open".to_owned())),
    ]);
    item.insert("lines".to_owned(), AttributeValue::N("2".to_owned()));
    item.insert("paid".to_owned(), AttributeValue::Bool(true));
    let object = table::to_object(item, &["PK", "SK"]).expect("object");
    assert_eq!(
        Value::Object(object),
        json!({ "status": "open", "lines": 2, "paid": true })
    );
}

#[test]
fn an_item_with_a_binary_attribute_is_not_an_object() {
    let item = Item::from([("blob".to_owned(), AttributeValue::B(vec![1, 2].into()))]);
    let failure = table::to_object(item, &[]).expect_err("binary");
    assert_eq!(failure.to_string(), "converting an item to JSON");
}

#[tokio::test]
async fn a_refused_condition_is_told_apart_from_other_failures() {
    let (client, _log) = dynamodb(|_, body| {
        if body.get("ConditionExpression").is_some() {
            error("ConditionalCheckFailedException")
        } else {
            error("ValidationException")
        }
    });
    let put = || {
        client
            .put_item()
            .table_name("orders")
            .set_item(Some(key(1)))
    };
    let refused = put()
        .condition_expression("attribute_not_exists(id)")
        .send()
        .await
        .expect_err("refused");
    let invalid = put().send().await.expect_err("invalid");
    assert!(table::is_conditional_failure(&refused));
    assert!(!table::is_conditional_failure(&invalid));
}

/// A span's name and its fields as text.
type Recorded = (String, Vec<(String, String)>);

/// Records the name and fields of every span created while it is the
/// default subscriber.
#[derive(Default)]
struct Spans(Mutex<Vec<Recorded>>);

struct Fields<'a>(&'a mut Vec<(String, String)>);

impl tracing::field::Visit for Fields<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

impl tracing::Subscriber for Spans {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut fields = Vec::new();
        span.record(&mut Fields(&mut fields));
        let mut spans = self.0.lock().expect("spans");
        spans.push((span.metadata().name().to_owned(), fields));
        tracing::span::Id::from_u64(spans.len() as u64)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn a_span_names_the_call_the_way_the_service_graph_expects() {
    let spans = Arc::new(Spans::default());
    tracing::subscriber::with_default(Arc::clone(&spans), || {
        drop(table::span("GetItem", "orders"));
    });
    let recorded = spans.0.lock().expect("spans");
    let (name, fields) = &recorded[0];
    assert_eq!(name, "dynamodb");
    let field = |name: &str| {
        fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(field("otel.name"), Some("DynamoDB.GetItem"));
    assert_eq!(field("otel.kind"), Some("CLIENT"));
    assert_eq!(field("db.system.name"), Some("aws.dynamodb"));
    assert_eq!(field("db.operation.name"), Some("GetItem"));
    assert_eq!(field("aws.dynamodb.table_names"), Some("orders"));
}

/// A key of the kind a page token carries.
fn start_key(order: &str) -> Item {
    [
        ("PK".to_owned(), AttributeValue::S("TENANT#t1".to_owned())),
        ("SK".to_owned(), AttributeValue::S(format!("ORDER#{order}"))),
    ]
    .into_iter()
    .collect()
}

#[test]
fn a_signed_token_round_trips_and_is_safe_in_a_url() {
    let secret = table::CursorKey::new(b"thirty-two bytes of cursor secret");
    let token = table::encode_cursor_signed(start_key("7"), &secret).expect("token");
    assert!(
        token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "{token}"
    );
    assert_eq!(
        table::decode_cursor_signed(&token, &secret),
        Some(start_key("7"))
    );
}

/// A client cannot move a read to a key of its choosing: every edit, a token
/// signed with another secret, an unsigned token and garbage all read as
/// "first page".
#[test]
fn a_forged_or_edited_signed_token_is_refused() {
    let secret = table::CursorKey::new(b"thirty-two bytes of cursor secret");
    let token = table::encode_cursor_signed(start_key("7"), &secret).expect("token");
    let (payload, tag) = token.split_once('.').expect("two parts");
    let forged_payload = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        r#"{"PK":"TENANT#t2","SK":"ORDER#1"}"#,
    );
    let other = table::CursorKey::new(b"another secret entirely, 32 bytes");
    for candidate in [
        format!("{forged_payload}.{tag}"),
        format!("{payload}.{}", &tag[1..]),
        table::encode_cursor_signed(start_key("7"), &other).expect("token"),
        table::encode_cursor(start_key("7")).expect("unsigned"),
        payload.to_owned(),
        "not a token".to_owned(),
        String::new(),
    ] {
        assert_eq!(
            table::decode_cursor_signed(&candidate, &secret),
            None,
            "{candidate}"
        );
    }
}

#[test]
fn a_cursor_key_never_prints_its_secret() {
    let secret = table::CursorKey::new(b"do-not-print-this-secret");
    assert_eq!(format!("{secret:?}"), "CursorKey(..)");
}

/// [`DynamoWindow`](davidrs::http::rate_limit::DynamoWindow), the rate-limit
/// counter kept in a table.
#[cfg(feature = "http")]
mod window {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use davidrs::http::rate_limit::DynamoWindow;
    use davidrs::http::{Counter as _, RateLimitConfig};
    use serde_json::{json, Value};

    use super::{calls, dynamodb, error, Call};

    const SEARCH: RateLimitConfig = RateLimitConfig::new("search", 60, Duration::from_secs(300));

    /// A store that answers every count with `count`.
    fn counting(count: &str) -> impl Fn(&str, &Value) -> (u16, Value) {
        let answer = json!({ "Attributes": { "requestCount": { "N": count } } });
        move |_, _| (200, answer.clone())
    }

    /// The start of the window a call counted in, in epoch milliseconds, from
    /// the sort key attribute `sort`.
    fn window_start(call: &Call, sort: &str) -> u64 {
        call.body["Key"][sort]["S"]
            .as_str()
            .and_then(|key| key.strip_prefix("WINDOW#"))
            .and_then(|start| start.parse().ok())
            .expect("a window key")
    }

    #[tokio::test]
    async fn a_request_is_counted_in_the_item_of_its_key_and_window() {
        let (client, log) = dynamodb(counting("7"));
        let before = SystemTime::now();
        let outcome = DynamoWindow::new(client, "limits")
            .hit("198.51.100.7", &SEARCH)
            .await
            .expect("counted");
        let call = &calls(&log)[0];
        let start = window_start(call, "SK");
        let resets_at = UNIX_EPOCH + Duration::from_millis(start + 300_000);
        assert_eq!(call.operation, "UpdateItem");
        assert_eq!(
            call.body["Key"]["PK"]["S"],
            "RATE_LIMIT#search#198.51.100.7"
        );
        assert_eq!(start % 300_000, 0, "windows are aligned to their length");
        assert!(resets_at > before && resets_at <= before + Duration::from_secs(301));
        assert_eq!(
            call.body["ExpressionAttributeValues"][":ttl"]["N"],
            ((start + 300_000) / 1000).to_string(),
            "the item expires when its window resets"
        );
        assert_eq!((outcome.remaining, outcome.resets_at), (53, resets_at));
    }

    /// A zero-length window would divide by zero.
    #[tokio::test]
    async fn a_window_shorter_than_a_millisecond_counts_as_one() {
        let (client, log) = dynamodb(counting("1"));
        let config = RateLimitConfig::new("search", 1, Duration::from_nanos(1));
        let outcome = DynamoWindow::new(client, "limits")
            .attributes("pk", "sk", "expires")
            .hit("198.51.100.7", &config)
            .await
            .expect("counted");
        let call = &calls(&log)[0];
        let start = window_start(call, "sk");
        assert_eq!(call.body["ExpressionAttributeNames"]["#ttl"], "expires");
        assert_eq!(
            outcome.resets_at,
            UNIX_EPOCH + Duration::from_millis(start + 1)
        );
        assert_eq!(outcome.remaining, 0);
    }

    #[tokio::test]
    async fn a_store_that_cannot_count_is_an_error() {
        let (client, _log) = dynamodb(|_, _| error("ResourceNotFoundException"));
        let failure = DynamoWindow::new(client, "limits")
            .hit("198.51.100.7", &SEARCH)
            .await
            .expect_err("refused");
        assert_eq!(failure.to_string(), "counting a request");
    }

    #[tokio::test]
    async fn a_count_the_store_did_not_return_is_an_error() {
        let (client, _log) = dynamodb(|_, _| (200, json!({})));
        let failure = DynamoWindow::new(client, "limits")
            .hit("198.51.100.7", &SEARCH)
            .await
            .expect_err("no count");
        assert_eq!(
            failure.to_string(),
            "the rate-limit counter was not returned"
        );
    }
}
