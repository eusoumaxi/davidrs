The complete template for a davidrs Lambda function: features, manifest, `main.rs`, handler, errors, deadlines, tests, workspace layout, release profile, local runs and deployment.

# Functions

## Choose the features

Start from what invokes the function, then add what the handler calls. Default features are empty and each feature links only what it names.

| The function… | Features | `main` ends with |
| --- | --- | --- |
| answers API Gateway or a Function URL with one response | `http` | `Api::new(op, policy, renderer).run(app, decode, handler)` |
| answers through a response stream (JSON or server-sent events) | `http-stream` | `StreamApi::new(op, policy, renderer).run(app, handler)` |
| also receives REST API (payload 1.0) events | add `apigw-rest` | either of the two above |
| consumes an SQS queue | `queue` (+ `queue-visibility` to delay a retry) | `davidrs::queue::run(app, handler)` |
| is an EventBridge rule target | `event` | `davidrs::event::run(app, handler)` |
| runs on a schedule | `schedule` | `davidrs::schedule::run(app, handler)` |
| is invoked directly, or by a source with no adapter | `runtime` | `davidrs::runtime::run(app, handler)` |
| streams a response without the HTTP pipeline | `streaming` | `davidrs::streaming::run(app, handler)` |
| serves MCP tools to AI clients | `mcp` (+ `mcp-openapi`) | `mcp::Server::new(name, version, policy)` … `.run(app)` |

Then: `dynamo`, `eventbridge` or `secrets` for those services (each enables `aws`, the SDK configuration), `client` for outbound HTTP, `auth` to verify bearer tokens in the function, `validate` and `problem` for HTTP input and errors, `logs`, `metrics` and `otel` for telemetry, `cache`, `compression` and `digest`. `test-support` belongs in `[dev-dependencies]` only. The [features chapter](https://docs.rs/davidrs/latest/davidrs/guide/features/index.html) lists what each one links.

## Cargo.toml

```toml
[package]
name = "orders-create"
version = "0.1.0"
edition = "2021"

[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["http", "logs"] }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

[dev-dependencies]
davidrs = { version = "0.1", default-features = false, features = ["test-support"] }
```

- The second `davidrs` entry adds `test-support` to test builds only; the production binary never contains it.
- `davidrs` does not re-export `tracing`, `serde_json`, `lambda_runtime` or the SDK crates. Add `tracing = "0.1"` to log from your own code, `serde_json = "1"` for `serde_json::Value`, `lambda_runtime = "1"` to name `MetadataPrelude` for `streaming::run`, and each SDK crate as `aws-sdk-<service> = { version = "1", default-features = false }`: `davidrs::aws::sdk_config` supplies its HTTP client and timer.
- Use Rust 1.98.1 or later.

## main.rs

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request, StatusCode};
use davidrs::{optional_env, required_env, Context, RuntimeError};
use serde::{Deserialize, Serialize};

/// Built once per cold start and shared by every invocation.
struct App {
    table: String,
    max_quantity: u32,
}

impl App {
    /// A missing or malformed variable fails the cold start, not a request.
    fn from_env() -> Result<Self, RuntimeError> {
        let max_quantity = match optional_env("MAX_QUANTITY") {
            Some(value) => value.parse().map_err(|_| {
                RuntimeError::Configuration("MAX_QUANTITY must be a whole number".to_owned())
            })?,
            None => 100,
        };
        Ok(Self {
            table: required_env("ORDERS_TABLE")?,
            max_quantity,
        })
    }
}

/// The body of `POST /orders`.
#[derive(Deserialize)]
struct NewOrder {
    item: String,
    quantity: u32,
}

#[derive(Serialize)]
struct Created {
    id: String,
}

/// Stands in for the storage call (DynamoDB: see aws-services.md).
async fn save(_table: &str, order: &NewOrder) -> Result<String, std::io::Error> {
    Ok(format!("{}-{}", order.item, order.quantity))
}

async fn create_order(
    app: Arc<App>,
    input: NewOrder,
    context: Context<()>,
) -> Result<(StatusCode, Json<Created>), Failure> {
    if input.quantity == 0 || input.quantity > app.max_quantity {
        return Err(Failure::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ERROR_INVALID_QUANTITY",
            "The quantity must be between 1 and the order limit",
        ));
    }
    let id = context
        .deadline()
        .child(Duration::from_secs(3))
        .run(save(&app.table, &input))
        .await?
        .map_err(|error| Failure::from_error("FAULT_ORDER_STORE", &error))?;
    Ok((StatusCode::CREATED, Json(Created { id })))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders-create")?;
    let app = Arc::new(App::from_env()?);
    Api::new("orders-create", Public, PlainErrors)
        .run(
            app,
            |request: &Request| request.json::<NewOrder>(),
            create_order,
        )
        .await
}
```

- `telemetry::init` (feature `logs`) installs logging, and X-Ray traces with `otel`, once per process. Hold the guard until `main` returns; without `logs`, leave the line out.
- `App::from_env()?` runs before the loop starts: a missing variable (`required_env`) or a malformed one (`RuntimeError::Configuration`) fails the cold start with the variable's name and never its value. `optional_env` treats a blank value as unset; `list_env` splits and trims a comma-separated list.
- The decoder is synchronous and bounded (1 MiB body limit by default): a bad body is a `400` before the handler runs. `Public` lets every caller through; put an API Gateway authorizer in front, or use an `Access` policy ([http.md](http.md)).
- `run` returns only when the Lambda loop itself fails, so its `Result` is what `main` returns.

## Handlers and Context

Every pipeline calls one shape: `async fn(Arc<App>, Input, Context<Scope>) -> Result<Output, Error>`.

- `Input` is what the HTTP decoder produced, or the trigger's typed payload.
- `context.invocation()` holds `request_id`, `trace_id`, `trace_root()`, `invoked_arn`, `tenant_id` and `started`; `context.deadline()` is the budget; `context.scope()` is what the HTTP policy established, and `()` for every other trigger.
- The handler's future must be `Send`: never hold a `std::sync::MutexGuard`, an `Rc` or a `RefCell` borrow across an `.await`.
- Keep the handler the business logic. Clients live in `App`; SDK calls use the SDK's own builders ([aws-services.md](aws-services.md)).

## Failure or RuntimeError

| Pipeline | Error type | What an `Err` does |
| --- | --- | --- |
| `Api`, `StreamApi`, MCP tools | `http::Failure` | rendered by the `ErrorRenderer`; every 5xx renders `InternalServerError` |
| `queue::run` | any `E: Display`, usually `RuntimeError` | the message is delivered again; only its id is logged |
| `event::run`, `schedule::run`, `runtime::run` | any `E: Display`, usually `RuntimeError` | an invocation error whose message is the `Display` text |

- 4xx: `Failure::new(status, "ERROR_…", "message the client may read")`, with a stable code.
- 5xx: `Failure::internal(code, detail)` or `Failure::from_error(code, &error)`, which keeps the whole source chain. The detail is never rendered, whatever the constructor; read it on purpose with `failure.internal_detail()`. `?` on a `RuntimeError` gives the same kind of `500`.
- `RuntimeError::message(..)`, `RuntimeError::other("what was being done", error)` (keeps the source) and `RuntimeError::Configuration(..)`. `davidrs::error_chain(&error)` prints the whole chain on one line. `RuntimeError` is non-exhaustive: match it with a `_` arm.
- A non-HTTP `Err` is what Lambda's retries, dead-letter queues and failure destinations act on: never catch a failure and return `Ok`. Its text reaches Lambda's logs and a synchronous caller, so keep secrets and personal data out of it.

## Deadlines

Each pipeline converts Lambda's deadline once into an absolute, monotonic `Deadline` and runs the handler under it minus a reserve kept to answer: 100 ms, or 1 s by default for `StreamApi`.

- `deadline.run(future).await` is `Err(RuntimeError::DeadlineExceeded { .. })` when the budget runs out first. The future is dropped at its next await, and not started at all when the budget is already gone.
- `deadline.child(budget)` is `budget` from now, never past the parent. `deadline.with_margin(margin)` ends `margin` earlier, never later. `deadline.remaining()` and `deadline.is_expired()` read it.
- Retries share one parent budget. A `runtime::run` handler, where `fetch_price` stands in for a call to another service:

```rust
async fn price(app: Arc<App>, item: String, context: Context<()>) -> Result<u64, RuntimeError> {
    let work = context.deadline().with_margin(Duration::from_millis(500));
    for _ in 0..3 {
        let attempt = work.child(Duration::from_secs(2));
        if let Ok(Ok(cents)) = attempt.run(fetch_price(&app, &item)).await {
            return Ok(cents);
        }
    }
    Err(RuntimeError::message("no price after three attempts"))
}
```

- Work that must survive cancellation (releasing a lease, saving progress) runs outside the bounded future, under the full deadline.
- A deadline cancels local waiting only: a write already sent may still land, so make writes idempotent. Synchronous code between awaits is not interrupted.

## Tests

A handler is an async function: call it directly with a context from `test_support::invocation("r-1")` (30 s of budget), `test_support::invocation_with_budget(..)` or, for the deadline path, `test_support::expired_invocation(..)`. `Api::handle` runs the whole HTTP pipeline (decoding, policy, handler, rendering) on a request from `test_support::get` or `test_support::post_json`. Appended to the `main.rs` above:

```rust
#[cfg(test)]
mod tests {
    use davidrs::http::codes;
    use davidrs::test_support;

    use super::*;

    fn app() -> Arc<App> {
        Arc::new(App {
            table: "orders".to_owned(),
            max_quantity: 10,
        })
    }

    #[tokio::test]
    async fn an_exhausted_budget_is_a_timeout() {
        let input = NewOrder {
            item: "book".to_owned(),
            quantity: 1,
        };
        let context = Context::new(test_support::expired_invocation("r-1"), ());
        let Err(failure) = create_order(app(), input, context).await else {
            panic!("expected a timeout");
        };
        assert_eq!(failure.code(), codes::TIMEOUT);
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_a_400() {
        let api = Api::new("orders-create", Public, PlainErrors);
        let decode = |request: &Request| request.json::<NewOrder>();
        let request = test_support::post_json("https://example.com/orders", "not json");
        let response = api.handle(app(), request, &decode, &create_order).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
```

- `request.path::<T>()` reads the path parameters API Gateway matched, and a request from `test_support::get` has none: test a path handler directly, or its route with `cargo lambda watch`.
- SQS batches run through `davidrs::queue::process` ([event-consumers.md](event-consumers.md)); streamed endpoints through `StreamApi::handle` ([http.md](http.md)).
- For AWS calls, give the SDK client the SDK's in-process test HTTP client instead of an account ([aws-services.md](aws-services.md)). No test needs credentials.
- `test-support` cannot build an authorized scope or skip a policy, by design: test a policy through `Api::handle`.

## A workspace of many functions

One function serves one operation, so a service is many small binaries in one Cargo workspace:

```text
Cargo.toml                 the workspace: members, shared dependency versions, [profile.release]
crates/app/                the application layer: Access policy, error catalog, renderer, finalizer
services/orders/core/      the domain: use cases, repositories, stored shapes
functions/orders-get/      one binary; main.rs wires one pipeline
functions/orders-create/
functions/orders-events/   an SQS or EventBridge consumer
```

The root `Cargo.toml`, the only place Cargo reads `[profile.*]` from:

```toml
[workspace]
members = ["crates/*", "services/*/core", "functions/*"]
resolver = "2"

[workspace.dependencies]
davidrs = { version = "0.1", default-features = false }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

[profile.release]
opt-level = "z"        # smallest code; the functions are I/O-bound
lto = true
codegen-units = 1
panic = "abort"
strip = "symbols"
```

A function adds exactly its features, `functions/orders-get/Cargo.toml`:

```toml
[package]
name = "orders-get"
version = "0.1.0"
edition = "2021"

[dependencies]
davidrs = { workspace = true, features = ["http", "logs"] }
orders-core = { path = "../../services/orders/core" }
serde.workspace = true
tokio.workspace = true

[package.metadata.lambda.watch.router]
"/orders/{id}" = "orders-get"
```

and its `src/main.rs` wires one pipeline (`Option<Json<_>>` answers `None` with a `404`):

```rust
use std::sync::Arc;

use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request};
use davidrs::{required_env, Context, RuntimeError};
use orders_core::Order;
use serde::Deserialize;

struct App {
    table: String,
}

#[derive(Deserialize)]
struct GetOrder {
    id: String,
}

async fn get(
    app: Arc<App>,
    input: GetOrder,
    context: Context<()>,
) -> Result<Option<Json<Order>>, Failure> {
    let order = orders_core::load(&app.table, &input.id, context.deadline()).await?;
    Ok(order.map(Json))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders-get")?;
    let app = Arc::new(App {
        table: required_env("ORDERS_TABLE")?,
    });
    Api::new("orders-get", Public, PlainErrors)
        .run(app, |request: &Request| request.path::<GetOrder>(), get)
        .await
}
```

The domain crate, `services/orders/core`, takes a `Deadline` and returns a `RuntimeError`:

```toml
[package]
name = "orders-core"
version = "0.1.0"
edition = "2021"

[dependencies]
davidrs = { workspace = true, features = ["runtime"] }
serde.workspace = true
```

```rust
//! The orders domain.

use std::time::Duration;

use davidrs::{Deadline, RuntimeError};
use serde::Serialize;

/// An order as the functions return it.
#[derive(Serialize)]
pub struct Order {
    /// The order id.
    pub id: String,
}

/// Stands in for a DynamoDB read, bounded by what is left of `deadline`.
pub async fn load(
    _table: &str,
    id: &str,
    deadline: Deadline,
) -> Result<Option<Order>, RuntimeError> {
    let read = async { Some(Order { id: id.to_owned() }) };
    deadline.child(Duration::from_secs(2)).run(read).await
}
```

- With no features, `davidrs` is its value types: `Deadline`, `Invocation`, `Context`, `RuntimeError` and the environment readers. `Deadline::run` needs a timer, which `runtime` (or another feature that awaits under a budget) brings. Without it, `cargo check -p orders-core` fails with "no method named `run` found for struct `Deadline`" while `cargo check -p orders-get` passes, because `orders-get` enables `http` and Cargo unifies the features.
- Check and build every package alone, the way deployment builds it:

```bash
cargo check -p orders-core
cargo check -p orders-get && cargo test -p orders-get
cargo lambda build --release --arm64 --package orders-get
```

## The release profile

The profile above makes the smallest binaries: a function that only answers HTTP ships as a zip of about 0.5–0.75 MB, an SQS consumer with `queue` and `logs` as about 0.5 MB, and the first AWS SDK client adds about 1.4 MB. Cargo Lambda adds `strip`, `lto`, `codegen-units` and `panic` itself when the profile leaves them out. A profile in a member manifest is ignored. With `panic = "abort"`, a panic ends the sandbox and the next invocation starts cold: return a `Failure` or a `RuntimeError` instead.

## Run locally

Install Cargo Lambda with `brew install cargo-lambda/tap/cargo-lambda` (which installs Zig too) or `pip3 install cargo-lambda`. For an HTTP function, give the local emulator the route API Gateway would match, in the function's `Cargo.toml`: without it, a `{id}` path parameter never reaches `request.path`.

```toml
[package.metadata.lambda.watch.router]
"/orders" = "orders-create"
```

```bash
cargo lambda watch                     # every binary crate on a local emulator, :9000
curl -X POST http://localhost:9000/orders -H 'content-type: application/json' -d '{"item":"book","quantity":2}'
cargo lambda invoke orders-events --data-file events/batch.json   # any other trigger
```

- A payload file holds what Lambda would deliver: an SQS event for `queue`, an EventBridge event for `event`, the bare payload for `schedule` and `runtime` ([event-consumers.md](event-consumers.md) shows each shape).
- Export the variables `App::from_env` reads in the shell that runs `cargo lambda watch`; for AWS calls, export credentials and `AWS_REGION` too ([aws-services.md](aws-services.md)).
- The emulator always sends a relative deadline of 600000 ms, read as ten minutes from now whatever the deployed timeout: test short deadlines with `test_support` instead.
- The emulator adds permissive CORS headers and answers preflights itself. Start it with `cargo lambda watch --disable-cors` to see what a `Cors` allowlist really answers.
- `cargo lambda watch` serves binary crates only. To run one of the repository's `examples/`, copy it into a crate made with `cargo lambda new`.

## Build and deploy

```bash
cargo lambda build --release --arm64                        # target/lambda/<function>/bootstrap
cargo lambda build --release --arm64 --output-format zip    # a zip for Terraform and other tools
cargo lambda deploy orders-create                           # quick manual deploy of one function
```

- Architecture `arm64` (Graviton costs less per millisecond), runtime `provided.al2023`, handler `bootstrap`.
- Infrastructure as code: `RustFunction` from `cargo-lambda-cdk` builds each binary during CDK synthesis (`architecture: Architecture.ARM_64`, `tracing: Tracing.ACTIVE`); AWS SAM uses `BuildMethod: rust-cargolambda` in the function's `Metadata`; Terraform and other tools deploy the zip.
- One role per function with exactly the actions and resources it uses: a read endpoint gets `dynamodb:GetItem` on one table, not `dynamodb:*`.
- Set the environment variables `App` reads. Secrets come from Secrets Manager through the `secrets` feature, never from plain variables.
- Memory: 256–512 MB suits most functions (CPU scales with memory). Timeout: what the operation needs; every pipeline stops its own work 100 ms before it (1 s for `StreamApi`) to answer.
- X-Ray: with `otel`, set the function's tracing mode to `Active`. In `PassThrough`, a function behind an HTTP API, a Function URL or SQS is never sampled.
- SQS: `FunctionResponseTypes: ["ReportBatchItemFailures"]` on the event source mapping, a dead-letter queue with `maxReceiveCount`, and a visibility timeout of at least six times the function timeout.
- EventBridge rules and schedules invoke asynchronously: a failed invocation is retried (twice by default), then sent to the on-failure destination or dead-letter queue you configure. Make handlers idempotent.
- HTTP: an HTTP API integration times out at 30 s (not raisable) and a buffered Lambda response is at most 6 MB. A Function URL with auth `NONE` has no gate but the function: cap it with reserved concurrency ([http.md](http.md)).

## Pitfalls

- Reading configuration or building clients inside a handler: a missing variable fails the first request instead of the deploy, and every invocation pays the setup.
- `tokio::time::timeout(Duration::from_secs(5), call)` per attempt: each retry gets a fresh allowance and the invocation overruns. Derive each attempt with `child` from one parent.
- Checking only the whole workspace: another member's features hide a missing one. Check `-p <function>`.
- Upstream error text as a 5xx message: it is replaced by `InternalServerError`. Put diagnostics in the detail.
- Returning `Ok` from a queue, event or schedule handler after a failure: Lambda cannot retry what it believes succeeded.
- `test-support` under `[dependencies]`: keep it in `[dev-dependencies]`.
- `unwrap()` on input or on an SDK answer: a panic aborts the sandbox and the next invocation starts cold.
- A route tested only through `test_support::get`: path parameters come from the gateway, so the decoder sees none.

## Guide

[Getting started](https://docs.rs/davidrs/latest/davidrs/guide/getting_started/index.html), [deployment](https://docs.rs/davidrs/latest/davidrs/guide/deployment/index.html), [architecture](https://docs.rs/davidrs/latest/davidrs/guide/architecture/index.html), [invocations](https://docs.rs/davidrs/latest/davidrs/guide/invocations/index.html), [features](https://docs.rs/davidrs/latest/davidrs/guide/features/index.html), [testing](https://docs.rs/davidrs/latest/davidrs/guide/testing/index.html), [security on AWS](https://docs.rs/davidrs/latest/davidrs/guide/aws_security/index.html).
