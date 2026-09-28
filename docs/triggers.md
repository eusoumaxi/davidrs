# Triggers

Three entry points serve the triggers that are neither HTTP nor SQS. Pick the row, enable that feature, and return `Err` for a real failure. Lambda's retries, on-failure destinations and dead-letter queues act on invocation errors. A handler that catches the failure and returns `Ok` looks successful, and the event is gone.

The three entry points are: [`event::run`](crate::event::run) for EventBridge rules, [`schedule::run`](crate::schedule::run) for schedules, and [`runtime::run`](crate::runtime::run) for everything else. Each one starts the native Lambda loop and calls your handler once per invocation.

| The function is invoked by | Use |
| --- | --- |
| an EventBridge rule on an event bus | [`event::run`](crate::event::run) |
| EventBridge Scheduler or a scheduled rule | [`schedule::run`](crate::schedule::run) |
| a direct `Invoke`, a workflow task, or a trigger no adapter models | [`runtime::run`](crate::runtime::run) |
| an SQS queue | [`queue::run`](crate::queue::run) |
| API Gateway or a Function URL | [`http::Api`](crate::http::Api) |

## The shared contract

All three take the application state and a handler of one shape:

```text
async fn(Arc<App>, Input, Context<()>) -> Result<Output, E>    where E: Into<Diagnostic>
```

The invocation runs in a fixed order:

1. The runtime deserializes the payload into `Input`. A payload that does not match is an invocation error, and the handler never runs.
2. The handler runs under the invocation deadline minus 100 ms, kept to post the answer.
3. `Ok` is serialized as the function's response (`null` for `event` and `schedule`). `Err` becomes an invocation error through `Into<`[`Diagnostic`](crate::runtime::Diagnostic)`>`, which sets its `errorType` and message: a [`RuntimeError`](crate::RuntimeError) gives its variant (`Configuration`, `LimitExceeded`, …), a `String` its text as the message, with its Rust type name as `errorType`, and an error of your own the `errorType` you choose in `From<YourError> for Diagnostic`, which a Step Functions `Retry` or `Catch` can match. A handler that overruns its budget becomes a `DeadlineExceeded` invocation error.

`run` itself returns only when the loop fails — for example when the Runtime API cannot be read — so its `Result` is what `main` returns.

**Why it is shaped like this.** An invocation error is what Lambda's retries, failure destinations and dead-letter queues act on. A handler that caught its own failure and returned `Ok` with an error inside would look successful to all of them, and the event would be gone. Deserializing before the handler means a payload that drifted from its type fails loudly at the boundary, not three branches later.

## Direct invocations: `runtime::run`

**What it is.** The native loop over any typed payload: `In` is deserialized from the invocation, `Out` is serialized as the response.

**Why it exists.** The plain `lambda_runtime` loop hands a handler the native context, whose deadline is epoch milliseconds that nothing enforces. `runtime::run` gives it the same [`Invocation`](crate::Invocation) and [`Deadline`](crate::Deadline) every other adapter gives, and turns an overrun into an invocation error instead of a process Lambda has to stop.

**How to use it.**

```rust,no_run
use std::sync::Arc;

use davidrs::{Context, RuntimeError};

struct App {
    rate_cents: u32,
}

#[derive(serde::Deserialize)]
struct Quote {
    order_id: String,
    quantity: u32,
}

#[derive(serde::Serialize)]
struct Priced {
    order_id: String,
    total_cents: u32,
}

async fn price(app: Arc<App>, quote: Quote, _: Context<()>) -> Result<Priced, RuntimeError> {
    if quote.quantity == 0 {
        return Err(RuntimeError::message("quantity must be positive"));
    }
    Ok(Priced {
        order_id: quote.order_id,
        total_cents: quote.quantity * app.rate_cents,
    })
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::runtime::run(Arc::new(App { rate_cents: 250 }), price).await
}
```

**Use cases.**

- A function another service calls with the SDK's `Invoke` and reads the answer of.
- A task in a workflow engine, whose output feeds the next step.
- A trigger with no adapter here (S3, SNS, a Kinesis or DynamoDB stream), read into your own type, a type from the `aws_lambda_events` crate, or [`serde_json::Value`].

**What it does not do.** No partial-batch reporting (use [`queue::run`](crate::queue::run)), no HTTP pipeline (use [`http::Api`](crate::http::Api)) and no streamed response (use [`streaming::run`](crate::streaming::run)). The handler's `Context` has no scope: authorizing a direct caller is IAM's job.

## EventBridge rules: `event::run` and `Event<T>`

**What it is.** A consumer of one EventBridge event per invocation. [`Event<T>`](crate::event::Event) is the envelope — `id`, `source`, `detail-type`, `time` — with the `detail` deserialized into your `T`.

**Why it exists.** Consumers written against `serde_json::Value` index into `detail` by string and find out at runtime that a publisher renamed a field. With a typed `detail`, an event that no longer matches is an invocation error, which Lambda's asynchronous retries and the function's on-failure destination or dead-letter queue act on, and the handler only ever sees events it understands.

**How to use it.**

```rust,no_run
use std::sync::Arc;

use davidrs::event::Event;
use davidrs::{Context, RuntimeError};

#[derive(serde::Deserialize)]
struct OrderShipped {
    order_id: String,
    carrier: String,
}

async fn on_shipped(
    _: Arc<()>,
    event: Event<OrderShipped>,
    context: Context<()>,
) -> Result<(), RuntimeError> {
    println!(
        "{} from {} ({}): {} shipped with {}",
        event.detail_type,
        event.source,
        context.invocation().request_id,
        event.detail.order_id,
        event.detail.carrier,
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::event::run(Arc::new(()), on_shipped).await
}
```

A handler is an ordinary async function, so a test calls it directly:

```rust
# use std::sync::Arc;
# use std::time::Duration;
# use davidrs::event::Event;
# use davidrs::{Context, Deadline, Invocation, RuntimeError};
# #[derive(serde::Deserialize)]
# struct OrderShipped { order_id: String }
# async fn on_shipped(_: Arc<()>, event: Event<OrderShipped>, _: Context<()>) -> Result<(), RuntimeError> {
#     if event.detail.order_id.is_empty() { Err(RuntimeError::message("no order")) } else { Ok(()) }
# }
# #[tokio::main(flavor = "current_thread")]
# async fn main() {
let event: Event<OrderShipped> = serde_json::from_value(serde_json::json!({
    "id": "e-1",
    "source": "shop.orders",
    "detail-type": "order.shipped",
    "detail": { "order_id": "o-1" }
}))
.expect("an EventBridge envelope");
let invocation = Invocation::new("r-1", Deadline::after(Duration::from_secs(5)));
assert!(on_shipped(Arc::new(()), event, Context::new(invocation, ())).await.is_ok());
# }
```

**Use cases.**

- Reacting to another service's domain events: send a receipt when an order is paid.
- Keeping a read model or a search index in step with its source.
- Fanning one event out to work that must not block its publisher.

**What it does not do.** It receives only: publishing is the `eventbridge` feature, so a consumer does not link a client it never calls. One invocation is one event — EventBridge does not batch. There is no deduplication: EventBridge delivers at least once, so a handler with side effects keys them by [`Event::id`](crate::event::Event::id) or makes them idempotent.

## Schedules: `schedule::run`

**What it is.** A scheduled handler over the payload the schedule is configured with, deserialized into your type. Use `()` for a schedule with no payload and [`serde_json::Value`] to accept anything.

**Why it exists.** A schedule's payload is written once, in infrastructure code, and rarely looked at again. When it drifts from what the handler expects, an untyped handler quietly takes a default branch every night. Typed, the mismatch fails the invocation, and Lambda's asynchronous retries and the function's on-failure destination or dead-letter queue report it.

**How to use it.**

```rust,no_run
use std::sync::Arc;

use davidrs::{Context, RuntimeError};

#[derive(serde::Deserialize)]
struct Purge {
    older_than_days: u32,
}

async fn purge(_: Arc<()>, input: Purge, context: Context<()>) -> Result<(), RuntimeError> {
    let budget = context.deadline().remaining();
    println!("purging items older than {} days within {budget:?}", input.older_than_days);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::schedule::run(Arc::new(()), purge).await
}
```

**Use cases.**

- Nightly cleanup of expired records.
- A periodic report or reconciliation against another system.
- Warming a cache before traffic arrives.

**What it does not do.** It does not know the schedule's expression or which run this is: put what the handler needs in the payload. It does not make a run resumable: a job longer than one invocation keeps its own progress and uses [`Deadline::with_margin`](crate::Deadline::with_margin) to stop in time to save it.
