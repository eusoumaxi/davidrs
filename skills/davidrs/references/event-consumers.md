Non-HTTP triggers with davidrs: SQS queues, EventBridge rules, schedules, direct invocations, sources without an adapter (S3, DynamoDB Streams, Kinesis, SNS) and streamed responses without the HTTP pipeline.

# Event consumers

## Choose the entry point

| Invoked by | Feature | `main` ends with | Handler input → output |
| --- | --- | --- | --- |
| an SQS event source mapping | `queue` | `davidrs::queue::run(app, handler)` | one `queue::Delivery` → `queue::Disposition` |
| an EventBridge rule | `event` | `davidrs::event::run(app, handler)` | `event::Event<T>` → `()` |
| EventBridge Scheduler or a scheduled rule | `schedule` | `davidrs::schedule::run(app, handler)` | the scheduled payload `T` → `()` |
| a direct `Invoke`, a workflow task, S3, DynamoDB Streams, Kinesis, SNS | `runtime` | `davidrs::runtime::run(app, handler)` | any `In` → any `Out` |
| a Function URL in `RESPONSE_STREAM` mode, read without the HTTP pipeline | `streaming` | `davidrs::streaming::run(app, handler)` | any `In` → `(MetadataPrelude, StreamBody)` |

Every handler is `async fn(Arc<App>, Input, Context<()>) -> Result<Output, E>` (the streaming handler returns its pair directly). For `event`, `schedule` and `runtime`, `E: Into<davidrs::runtime::Diagnostic>`: `RuntimeError` (its variant becomes the `errorType`), `String`, `&'static str`, `std::io::Error`, or your own error with `impl From<YourError> for Diagnostic` to choose the `errorType` a Step Functions `Retry`/`Catch` matches. For `queue`, `E: Display`. The contract is the same for all of them:

1. The runtime deserializes the payload into `Input`. A payload that does not match is an invocation error, and the handler never runs.
2. The handler runs under the invocation deadline minus 100 ms, kept to post the answer.
3. `Ok` is serialized as the response (`null` for `event` and `schedule`). `Err` becomes an invocation error whose message is the `Display` text; an overrun becomes `deadline exceeded after … ms`. The queue adapter turns both into per-message failures instead.

`run` returns only when the Lambda loop itself fails, so its `Result` is what `main` returns.

## Rules

- Return `Err` for every failure. Lambda's retries, redrive policies, dead-letter queues and on-failure destinations act on errors; an `Ok` with a failure inside loses the event.
- Every source here delivers at least once: SQS, EventBridge, S3 and SNS may deliver a message twice, and every retry repeats one. Make each side effect idempotent: a conditional write keyed by the business id, or the event id, message id, object key and version, or sequence number.
- Deserialize the payload into a typed struct at the boundary, never index a `serde_json::Value`: a payload that drifted then fails loudly as an invocation error.
- Bound every call with the context's deadline (`child`, `with_margin`, `run`); a timeout cannot undo a write already sent.
- Log a classification (message id, receive count, a fixed reason), never the error text or the body: either may hold data the application must not log.
- The payload type needs `DeserializeOwned + Send`, the output `Serialize`, and the handler's future `Send`.

## SQS: `queue::run`

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["queue", "logs"] }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
tracing = "0.1"

[dev-dependencies]
serde_json = "1"
```

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::queue::{Delivery, Disposition};
use davidrs::{required_env, Context, RuntimeError};
use serde::Deserialize;

struct App {
    table: String,
}

/// The body every message on the queue carries.
#[derive(Deserialize)]
struct OrderPaid {
    order_id: String,
    settled: bool,
}

/// Stands in for an idempotent write: a conditional put keyed by the order id,
/// so a redelivered message changes nothing.
async fn mark_paid(_table: &str, _order_id: &str) -> Result<(), std::io::Error> {
    Ok(())
}

async fn consume(
    app: Arc<App>,
    delivery: Delivery,
    context: Context<()>,
) -> Result<Disposition, RuntimeError> {
    let order: OrderPaid = delivery.json().inspect_err(|_| {
        tracing::warn!(
            message_id = %delivery.message_id,
            receive_count = delivery.receive_count(),
            "malformed order message"
        );
    })?;
    if !order.settled {
        return Ok(Disposition::Retry);
    }
    context
        .deadline()
        .child(Duration::from_secs(5))
        .run(mark_paid(&app.table, &order.order_id))
        .await?
        .map_err(|error| RuntimeError::other("recording the payment", error))?;
    Ok(Disposition::Delete)
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders-paid")?;
    let app = Arc::new(App {
        table: required_env("ORDERS_TABLE")?,
    });
    davidrs::queue::run(app, consume).await
}
```

Each message gets one outcome, and the adapter answers Lambda with the partial-batch response `{"batchItemFailures": [{"itemIdentifier": "<messageId>"}]}` listing only the messages SQS must deliver again:

| The handler | The message | The rest of the batch |
| --- | --- | --- |
| returns `Ok(Disposition::Delete)` | is deleted | goes on |
| returns `Ok(Disposition::Retry)` | is delivered again, not logged | goes on; on a FIFO queue, stops and is delivered again |
| returns `Err(..)` | is delivered again, its id logged (with `logs`) | goes on; on a FIFO queue, stops and is delivered again |
| runs past the deadline | is delivered again | stops and is delivered again |

- Messages run one at a time, in order, each under the invocation deadline minus 100 ms. A message not attempted before the budget ran out is reported, never silently deleted, and the function answers before Lambda's own timeout.
- A queue whose ARN ends in `.fifo` stops at the first `Retry` or `Err` and reports every later message, so a message group never runs out of order.
- `Delivery` has `message_id`, `receipt_handle`, `body`, `attributes` and `event_source_arn`. `delivery.json::<T>()` decodes the body, with an error that names the message; `delivery.receive_count()` reads `ApproximateReceiveCount` and answers `1` when it is absent or unreadable.
- A failed message is logged by id only, never with the error text: log your own classification first, as above.
- A panic fails the whole invocation, and SQS delivers the whole batch again.
- There is no parallelism, no dead-letter logic and no sending or deleting: tune the batch size and the function's concurrency, give the queue a redrive policy, and call `aws-sdk-sqs` directly for other SQS operations.

### The event source mapping

- `FunctionResponseTypes: ["ReportBatchItemFailures"]`. Without it Lambda ignores the response and deletes the whole batch, failed messages included.
- A dead-letter queue with a `maxReceiveCount` on the source queue: that is where a message that keeps failing ends up. `receive_count()` only shows how close it is.
- A queue visibility timeout of at least six times the function timeout.
- The execution role needs `sqs:ReceiveMessage`, `sqs:DeleteMessage` and `sqs:GetQueueAttributes` on the queue, and `sqs:ChangeMessageVisibility` to use `Visibility`.

### Delaying a retry: `Visibility`

An upstream that answers "try again in 4 s" should not wait out the queue's 30 s. Set the message's visibility (below, the upstream's delay, growing with each delivery), then return `Disposition::Retry`. This needs the `queue-visibility` feature and the SQS client crate:

```toml
[dependencies]
aws-sdk-sqs = { version = "1", default-features = false }
davidrs = { version = "0.1", default-features = false, features = ["queue-visibility"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::aws::{sdk_config, Trust};
use davidrs::queue::{Delivery, Disposition, Visibility, MAX_VISIBILITY_TIMEOUT};
use davidrs::{required_env, Context, RuntimeError};

struct App {
    sqs: aws_sdk_sqs::Client,
    queue_url: String,
}

/// Stands in for an upstream call that can answer "try again in N seconds".
async fn forward(_body: &str) -> Result<(), Duration> {
    Ok(())
}

async fn consume(
    app: Arc<App>,
    delivery: Delivery,
    _context: Context<()>,
) -> Result<Disposition, RuntimeError> {
    let Err(retry_after) = forward(&delivery.body).await else {
        return Ok(Disposition::Delete);
    };
    let wait = retry_after
        .saturating_mul(delivery.receive_count())
        .min(MAX_VISIBILITY_TIMEOUT);
    Visibility::new(app.queue_url.as_str(), &delivery)
        .set(&app.sqs, wait)
        .await?;
    Ok(Disposition::Retry)
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let config = sdk_config(Trust::NativeRoots)?;
    let app = Arc::new(App {
        sqs: aws_sdk_sqs::Client::new(&config),
        queue_url: required_env("QUEUE_URL")?,
    });
    davidrs::queue::run(app, consume).await
}
```

- `Visibility` is built only from a `Delivery`, so a message id can never be passed where SQS needs the receipt handle. The queue URL comes from configuration: the event carries the queue's ARN, not its URL.
- The delay counts from the call, in whole seconds rounded up, at most `MAX_VISIBILITY_TIMEOUT` (12 hours); a longer one is refused with `RuntimeError::Configuration` before SQS is called.
- A receipt handle is valid only during the current visibility window: a late call fails.
- `sdk_config` builds the SDK configuration from what Lambda injects ([aws-services.md](aws-services.md)).

### Test a consumer

`davidrs::queue::process(app, batch, &invocation, &handler)` is the adapter without the Lambda loop. `Batch` is non-exhaustive, so build it from JSON, exactly as Lambda delivers it. Appended to the consumer's `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use davidrs::queue::Batch;
    use davidrs::{Deadline, Invocation};
    use serde_json::{json, Value};

    use super::*;

    fn record(id: &str, body: &str) -> Value {
        json!({ "messageId": id, "receiptHandle": format!("handle-{id}"), "body": body })
    }

    #[tokio::test]
    async fn only_the_unsettled_and_the_malformed_orders_come_back() {
        let batch: Batch = serde_json::from_value(json!({ "Records": [
            record("m-1", r#"{"order_id":"o-1","settled":true}"#),
            record("m-2", r#"{"order_id":"o-2","settled":false}"#),
            record("m-3", "not json")
        ] }))
        .expect("an SQS event");
        let app = Arc::new(App {
            table: "orders".to_owned(),
        });
        let invocation = Invocation::new("r-1", Deadline::after(Duration::from_secs(5)));

        let response = davidrs::queue::process(app, batch, &invocation, &consume).await;

        let failed = json!([{ "itemIdentifier": "m-2" }, { "itemIdentifier": "m-3" }]);
        assert_eq!(
            serde_json::to_value(&response).expect("json"),
            json!({ "batchItemFailures": failed })
        );
    }
}
```

- Add `"eventSourceARN": "arn:aws:sqs:eu-west-1:123456789012:orders.fifo"` to the records to test the FIFO stop, and `"attributes": { "ApproximateReceiveCount": "3" }` to test a backoff.
- Pass an invocation whose deadline has passed (`davidrs::test_support::expired_invocation`, feature `test-support` in `[dev-dependencies]`) to see every message reported.
- For `cargo lambda invoke <function> --data-file batch.json`, the file holds the same shape: `{"Records": [{"messageId": "m-1", "receiptHandle": "h-1", "body": "…"}]}`.

## EventBridge rules: `event::run`

Feature `event`. One invocation is one event; `Event<T>` is the envelope (`id`, `source`, `detail_type`, `time`) with `detail` deserialized into `T`.

```rust
use std::sync::Arc;

use davidrs::event::Event;
use davidrs::{Context, RuntimeError};
use serde::Deserialize;

struct App;

/// The `detail` of an `order.shipped` event.
#[derive(Deserialize)]
struct OrderShipped {
    order_id: String,
}

/// Stands in for a side effect keyed by the event id, so a second delivery
/// of the same event sends nothing.
async fn send_receipt(_app: &App, _event_id: &str, _order_id: &str) -> Result<(), std::io::Error> {
    Ok(())
}

async fn on_shipped(
    app: Arc<App>,
    event: Event<OrderShipped>,
    _context: Context<()>,
) -> Result<(), RuntimeError> {
    send_receipt(&app, &event.id, &event.detail.order_id)
        .await
        .map_err(|error| RuntimeError::other("sending the receipt", error))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::event::run(Arc::new(App), on_shipped).await
}
```

A test builds the event from JSON (the key is `detail-type`) and calls the handler directly, with `test-support` and `serde_json` as dev-dependencies:

```rust
#[cfg(test)]
mod tests {
    use davidrs::test_support;

    use super::*;

    #[tokio::test]
    async fn a_shipped_order_gets_a_receipt() {
        let event: Event<OrderShipped> = serde_json::from_value(serde_json::json!({
            "id": "e-1",
            "source": "orders",
            "detail-type": "order.shipped",
            "detail": { "order_id": "o-1" }
        }))
        .expect("an EventBridge event");
        let context = Context::new(test_support::invocation("r-1"), ());
        assert!(on_shipped(Arc::new(App), event, context).await.is_ok());
    }
}
```

- An event whose `detail` does not deserialize, or a handler `Err`, is an invocation error. EventBridge invokes asynchronously: Lambda retries a failed event (twice by default), then sends it to the function's on-failure destination or dead-letter queue. Configure one.
- EventBridge delivers at least once; a retry keeps the same `event.id`, so key side effects by it.
- This only receives. Publishing is the `eventbridge` feature ([aws-services.md](aws-services.md)).

## Schedules: `schedule::run`

Feature `schedule`. The handler gets the payload the schedule is configured with, deserialized into `T`:

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::{Context, RuntimeError};
use serde::Deserialize;

struct App;

/// The input the schedule is configured with: `{"older_than_days": 30}`.
#[derive(Deserialize)]
struct Purge {
    older_than_days: u32,
}

/// Stands in for deleting one page of expired items; `false` once none are left.
async fn purge_page(_app: &App, _older_than_days: u32) -> Result<bool, std::io::Error> {
    Ok(false)
}

/// Deletes page by page until none are left or 2 s of the budget remain.
async fn purge(app: Arc<App>, input: Purge, context: Context<()>) -> Result<(), RuntimeError> {
    if input.older_than_days == 0 {
        return Err(RuntimeError::message("older_than_days must be positive"));
    }
    let work = context.deadline().with_margin(Duration::from_secs(2));
    while !work.is_expired() {
        let more = work
            .run(purge_page(&app, input.older_than_days))
            .await?
            .map_err(|error| RuntimeError::other("purging a page", error))?;
        if !more {
            break;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::schedule::run(Arc::new(App), purge).await
}
```

- Give every schedule an explicit input that matches `T`. Use `()` only for a JSON `null` payload, and `serde_json::Value` to accept anything. A scheduled rule with no input configured delivers its whole `Scheduled Event` envelope; read that with `event::run` and `Event<serde_json::Value>`.
- The handler does not know the schedule expression or which run this is: put what it needs in the payload.
- A job longer than one invocation keeps its own progress and stops in time with `with_margin`. Here a page cut off by the budget fails the invocation and Lambda's retry carries on, which is safe because deleting expired items twice is harmless.
- Failures follow Lambda's asynchronous retries and the function's on-failure destination or dead-letter queue, as for EventBridge.

## Direct invocations: `runtime::run`

Feature `runtime`. `In` is deserialized from the payload and `Out` serialized as the response:

```rust
use std::sync::Arc;

use davidrs::{Context, RuntimeError};
use serde::{Deserialize, Serialize};

struct App {
    rate_cents: u64,
}

#[derive(Deserialize)]
struct Quote {
    order_id: String,
    quantity: u64,
}

#[derive(Serialize)]
struct Priced {
    order_id: String,
    total_cents: u64,
}

async fn price(app: Arc<App>, quote: Quote, _context: Context<()>) -> Result<Priced, RuntimeError> {
    if quote.quantity == 0 {
        return Err(RuntimeError::message("quantity must be positive"));
    }
    let total_cents = quote
        .quantity
        .checked_mul(app.rate_cents)
        .ok_or_else(|| RuntimeError::message("the total is out of range"))?;
    Ok(Priced {
        order_id: quote.order_id,
        total_cents,
    })
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::runtime::run(Arc::new(App { rate_cents: 250 }), price).await
}
```

- A synchronous caller (`Invoke`, a workflow task) receives an `Err` as a function error whose message is the `Display` text: keep secrets and personal data out of it.
- The context has no scope: who may invoke the function is decided by IAM (`lambda:InvokeFunction`), not by the handler.
- There is no partial-batch reporting, HTTP pipeline or streamed response here: use `queue::run`, `Api` ([http.md](http.md)) or `streaming::run`.

## Sources without an adapter: S3, DynamoDB Streams, Kinesis, SNS

Use `runtime::run` with a payload type that implements `DeserializeOwned + Send`: either the event types of the `aws_lambda_events` crate, or your own struct with only the fields you read.

### With `aws_lambda_events`

Its default features enable every event type, so turn them off and enable only the source's:

```toml
[dependencies]
aws_lambda_events = { version = "1", default-features = false, features = ["s3"] }
davidrs = { version = "0.1", default-features = false, features = ["runtime"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

| Source | Feature | Payload type |
| --- | --- | --- |
| S3 notifications | `s3` | `aws_lambda_events::s3::S3Event` |
| SNS | `sns` | `aws_lambda_events::sns::SnsEvent`; `record.sns.message` is the published string |
| DynamoDB Streams | `dynamodb` | `aws_lambda_events::dynamodb::Event`; each record's `dynamodb` object is the field `change` |
| Kinesis | `kinesis` | `aws_lambda_events::kinesis::KinesisEvent`; `record.kinesis.data` is already base64-decoded |
| stream partial-batch responses | `streams` | `aws_lambda_events::streams::KinesisEventResponse` (`add_failure(sequence_number)`), `DynamoDbEventResponse` |

```rust
use std::sync::Arc;

use aws_lambda_events::s3::S3Event;
use davidrs::{Context, RuntimeError};

struct App;

/// Stands in for processing one object, idempotently; `key` is still URL-encoded.
async fn index_object(_app: &App, _bucket: &str, _key: &str) -> Result<(), std::io::Error> {
    Ok(())
}

async fn on_upload(
    app: Arc<App>,
    event: S3Event,
    _context: Context<()>,
) -> Result<(), RuntimeError> {
    for record in event.records {
        let (Some(bucket), Some(key)) = (record.s3.bucket.name, record.s3.object.key) else {
            return Err(RuntimeError::message(
                "an S3 record without a bucket or key",
            ));
        };
        index_object(&app, &bucket, &key)
            .await
            .map_err(|error| RuntimeError::other("indexing an uploaded object", error))?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::runtime::run(Arc::new(App), on_upload).await
}
```

- S3 and SNS invoke asynchronously: an `Err` gets Lambda's retries and the function's on-failure destination or dead-letter queue.
- An S3 object key arrives URL-encoded (`red flower.jpg` is `red+flower.jpg`): decode it before calling S3. S3 may notify twice, so key the work by bucket, key and version.
- An SNS message body is a string: deserialize `record.sns.message` into your type with `serde_json`.

### With your own struct, and partial batches from a stream

A struct with only the fields the handler reads needs no extra crate beyond `serde` (and `serde_json` here, for the keys). DynamoDB Streams and Kinesis mappings with `ReportBatchItemFailures` accept the same partial-batch response as SQS, naming records by sequence number:

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::{Context, RuntimeError};
use serde::{Deserialize, Serialize};

struct App;

/// The part of a DynamoDB Streams event this consumer reads.
#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "Records")]
    records: Vec<StreamRecord>,
}

#[derive(Deserialize)]
struct StreamRecord {
    #[serde(rename = "eventName")]
    event_name: String,
    dynamodb: Change,
}

#[derive(Deserialize)]
struct Change {
    #[serde(rename = "SequenceNumber")]
    sequence_number: String,
    #[serde(rename = "Keys")]
    keys: serde_json::Value,
}

/// The partial-batch response: `{"batchItemFailures": [{"itemIdentifier": "..."}]}`.
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchFailures {
    batch_item_failures: Vec<ItemFailure>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemFailure {
    item_identifier: String,
}

/// Stands in for applying one change, idempotently.
async fn apply(
    _app: &App,
    _event_name: &str,
    _keys: &serde_json::Value,
) -> Result<(), std::io::Error> {
    Ok(())
}

/// Stops at the first record that fails or runs out of time, and reports it:
/// Lambda retries the shard from that sequence number.
async fn on_changes(
    app: Arc<App>,
    event: StreamEvent,
    context: Context<()>,
) -> Result<BatchFailures, RuntimeError> {
    let work = context.deadline().with_margin(Duration::from_millis(500));
    let mut failures = BatchFailures::default();
    for record in event.records {
        let applied = work
            .run(apply(&app, &record.event_name, &record.dynamodb.keys))
            .await;
        if !matches!(applied, Ok(Ok(()))) {
            failures.batch_item_failures.push(ItemFailure {
                item_identifier: record.dynamodb.sequence_number,
            });
            break;
        }
    }
    Ok(failures)
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::runtime::run(Arc::new(App), on_changes).await
}
```

- Records of a shard arrive in order. Report the first record that failed and stop: Lambda retries from it, so processing later records would repeat them. An empty list means the whole batch succeeded.
- An `Err`, or an overrun of the invocation, retries the whole batch. By default a failing batch is retried until it succeeds or its records expire, which blocks the shard: set `MaximumRetryAttempts`, `BisectBatchOnFunctionError` and an on-failure destination on the event source mapping.
- Kinesis works the same way with `kinesis.sequenceNumber` and base64 `kinesis.data`, or with `KinesisEvent` and `KinesisEventResponse::add_failure` from `aws_lambda_events` (features `kinesis` and `streams`).

## Streamed responses without the HTTP pipeline: `streaming::run`

`streaming::run` answers through a Lambda response stream without the HTTP pipeline: the handler reads the invocation payload as any `In` (`serde_json::Value` for the raw event) and returns the head, a `MetadataPrelude` with the status, headers and cookies, and a `StreamBody`. For an HTTP endpoint prefer `StreamApi` ([http.md](http.md)), which adds CORS, content negotiation, the policy and rendered failures. The prelude type comes from `lambda_runtime`, which you name yourself:

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["streaming"] }
lambda_runtime = "1"
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::streaming::StreamBody;
use davidrs::{Context, RuntimeError};
use lambda_runtime::MetadataPrelude;

/// Stands in for reading one page from a paginated upstream.
async fn fetch_page(page: u32) -> String {
    format!("data: page {page}\n\n")
}

async fn export(
    _app: Arc<()>,
    _event: serde_json::Value,
    context: Context<()>,
) -> (MetadataPrelude, StreamBody) {
    let mut head = MetadataPrelude::default();
    head.headers.insert(
        "content-type",
        "text/event-stream".parse().expect("static header"),
    );
    let deadline = context.deadline().with_margin(Duration::from_secs(1));
    let body = StreamBody::spawn(4, deadline, |producer| async move {
        for page in 1..=3 {
            let rows = tokio::select! {
                () = producer.cancelled() => return,
                rows = fetch_page(page) => rows,
            };
            if !producer.send(rows).await {
                return;
            }
        }
    });
    (head, body)
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::streaming::run(Arc::new(()), export).await
}
```

- The body owns its producer. Dropping it (the client left) cancels the producer, which gets 50 ms to stop before its future is dropped; the producer's own deadline, here the invocation's minus 1 s to flush the last frames, ends the stream even while a client keeps reading. `send` returning `false` means nobody is reading: stop.
- `StreamBody::spawn`'s capacity (4 here) is how far the producer may run ahead of the reader. `Producer::fail(error)` ends the stream with an error; a panic surfaces as a final error item, not as a stream that looks complete.
- The handler returns the pair itself, not a `Result`: a failure found before streaming sets the prelude's `status_code`, with a short body from `StreamBody::once(Some(..))`.
- Writes the producer already made stay made when the stream is cancelled: make them idempotent.

## Pitfalls

- An SQS mapping without `ReportBatchItemFailures`: every failed message is deleted as if it succeeded.
- Returning `Err` from a hand-written SQS loop instead of using `queue::run`: the whole batch, successes included, comes back.
- `schedule::run` with `T = ()` behind a scheduled rule that sends its envelope: every run fails to deserialize.
- A visibility timeout shorter than the function timeout: SQS delivers a message again while the first invocation still works on it.
- Keying idempotency by SQS message id when a producer may send the same business event twice: key by the business id instead.
- Processing past the first failed record of a stream batch: the retry repeats everything after it.
- Logging `error` or `delivery.body` on failure: log the message id and a fixed reason.

## Guide

[Queues](https://docs.rs/davidrs/latest/davidrs/guide/queues/index.html), [triggers](https://docs.rs/davidrs/latest/davidrs/guide/triggers/index.html), [streaming](https://docs.rs/davidrs/latest/davidrs/guide/streaming/index.html), [invocations](https://docs.rs/davidrs/latest/davidrs/guide/invocations/index.html), [deployment](https://docs.rs/davidrs/latest/davidrs/guide/deployment/index.html).
