# Queues

[`queue::run`](crate::queue::run) runs an SQS consumer on the native Lambda loop. Your handler receives one [`Delivery`](crate::queue::Delivery) at a time and returns a [`Disposition`](crate::queue::Disposition); the adapter turns the results into the partial-batch response Lambda expects, a [`BatchResponse`](crate::queue::BatchResponse) listing only the messages SQS must deliver again. [`Visibility`](crate::queue::Visibility), with the `queue-visibility` feature, changes one message's visibility timeout so a retry can come sooner or later than the queue's default.

## Why it exists

A hand-written SQS consumer tends to fail in one of four ways:

- **The whole batch comes back.** Returning `Err` from the function makes SQS redeliver every message of the batch, including the nine that succeeded. Their side effects run twice. Here a failure is reported for its own message only.
- **Unattempted messages are deleted.** A loop that stops when time runs out and reports only the failures it saw tells Lambda that the messages it never reached succeeded, and SQS deletes them. Here every message not attempted is reported, so nothing is lost.
- **FIFO order breaks.** Carrying on after a failure in a FIFO batch processes later messages of the group before the failed one is delivered again. On a queue whose ARN ends in `.fifo`, the adapter stops at the first failure and reports every later message.
- **A message id is used as a receipt handle.** Changing visibility needs the receipt handle of this delivery, not the message id. `Visibility` can only be built from a delivery, so the two cannot be confused.

## How to use it

The handler has the shape every trigger shares. `Ok(Disposition::Delete)` lets SQS delete the message, `Ok(Disposition::Retry)` asks for it again without calling it an error, and `Err` asks for it again and logs its message id.

```rust,no_run
use std::sync::Arc;

use davidrs::queue::{Delivery, Disposition};
use davidrs::{Context, RuntimeError};

/// The shared state of the consumer.
struct App;

/// The body every message on this queue carries.
#[derive(serde::Deserialize)]
struct OrderPlaced {
    order_id: String,
    paid: bool,
}

async fn consume(_app: Arc<App>, delivery: Delivery, _context: Context<()>) -> Result<Disposition, RuntimeError> {
    let order: OrderPlaced = delivery.json()?;
    if order.order_id.is_empty() {
        return Err(RuntimeError::message("an order without an id"));
    }
    if !order.paid {
        return Ok(Disposition::Retry);
    }
    Ok(Disposition::Delete)
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::queue::run(Arc::new(App), consume).await
}
```

The partial response only takes effect when the event source mapping has `ReportBatchItemFailures` in its `FunctionResponseTypes`. Without it, Lambda ignores the response and deletes the whole batch after a successful invocation.

Every outcome maps to one rule:

| The handler | The message | The rest of the batch |
| --- | --- | --- |
| returns `Ok(Delete)` | is deleted | goes on |
| returns `Ok(Retry)` | is delivered again | goes on; on a FIFO queue, stops and is delivered again |
| returns `Err` | is delivered again, its id logged with the `logs` feature | goes on; on a FIFO queue, stops and is delivered again |
| runs past the deadline | is delivered again | stops and is delivered again |

Each message runs under the invocation deadline minus 100 ms, kept to send the response. When a handler is still running at that point it is dropped, and the adapter answers in time instead of letting Lambda stop the function, which would bring the whole batch back.

[`process`](crate::queue::process) is the same logic without the runtime, which makes a handler easy to test. A batch is built from its JSON, exactly as Lambda delivers it:

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::queue::{Batch, Delivery, Disposition};
use davidrs::{Context, Deadline, Invocation};

async fn consume(_app: Arc<()>, delivery: Delivery, _context: Context<()>) -> Result<Disposition, String> {
    match delivery.body.as_str() {
        "ok" => Ok(Disposition::Delete),
        other => Err(format!("unexpected body {other}")),
    }
}

# #[tokio::main(flavor = "current_thread")]
# async fn main() {
let batch: Batch = serde_json::from_value(serde_json::json!({
    "Records": [
        { "messageId": "m-1", "receiptHandle": "handle-1", "body": "ok" },
        { "messageId": "m-2", "receiptHandle": "handle-2", "body": "garbled" },
        { "messageId": "m-3", "receiptHandle": "handle-3", "body": "ok" }
    ]
}))
.expect("batch");
let invocation = Invocation::new("request-1", Deadline::in_from_now(Duration::from_secs(5)));

let response = davidrs::queue::process(Arc::new(()), batch, &invocation, &consume).await;

assert_eq!(
    serde_json::to_value(&response).expect("json"),
    serde_json::json!({ "batchItemFailures": [{ "itemIdentifier": "m-2" }] })
);
# }
```

[`Delivery::receive_count`](crate::queue::Delivery::receive_count) reads `ApproximateReceiveCount`. When the attribute is missing or unreadable it answers `1`, so an unknown count reads as a first attempt, never as an exhausted one.

To choose when a message comes back, set its visibility before returning `Retry`. The delay counts from the call; SQS counts whole seconds, up to [`MAX_VISIBILITY_TIMEOUT`](crate::queue::MAX_VISIBILITY_TIMEOUT) (12 hours), and a longer delay is refused before SQS is called:

```rust,no_run
use std::sync::Arc;
use std::time::Duration;

use davidrs::queue::{Delivery, Disposition, Visibility, MAX_VISIBILITY_TIMEOUT};
use davidrs::{Context, RuntimeError};

/// The consumer's state: an SQS client and the queue it reads.
struct App {
    sqs: aws_sdk_sqs::Client,
    queue_url: String,
}

async fn consume(app: Arc<App>, delivery: Delivery, _context: Context<()>) -> Result<Disposition, RuntimeError> {
    let wait = (Duration::from_secs(4) * delivery.receive_count()).min(MAX_VISIBILITY_TIMEOUT);
    Visibility::new(app.queue_url.as_str(), &delivery).set(&app.sqs, wait).await?;
    Ok(Disposition::Retry)
}
```

A receipt handle is only valid during the current visibility window, so a call made after it has closed fails.

## Use cases

- A batch where one message has a malformed body: only that message comes back, and after the queue's `maxReceiveCount` the redrive policy moves it to the dead-letter queue.
- An upstream that answers "try again in 4 s": set the visibility to 4 s and return `Retry`, instead of waiting out the queue's 30 s.
- A FIFO queue keyed by customer, where an update must never overtake the one before it.
- A backoff that grows with each delivery, computed from `receive_count`.

## What it does not do

- **No parallel work.** Messages run one at a time, in order. Tune the batch size and the function's concurrency instead.
- **No panic handling.** A handler that panics fails the invocation, and SQS delivers the whole batch again.
- **No dead-letter logic.** Giving up after a number of attempts is the queue's redrive policy; `receive_count` only lets you see how close a message is.
- **No error text in the logs.** A failed message is logged by id only, because an error may carry data the application must not log. Log your own classification before returning `Err`.
- **No sending or deleting.** Other SQS calls use `aws-sdk-sqs` directly.
