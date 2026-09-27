# Getting started

This chapter builds one HTTP function from an empty crate, then shows how to choose features for other triggers and how to run a function locally.

## Add the dependency

Use Rust 1.98.1 or later. Depend on the crate with default features off and only the capabilities the function uses:

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["http", "logs"] }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros"] }
```

`lambda_runtime` already brings Tokio's multi-threaded runtime, so the function's own manifest only needs Tokio's `macros` for `#[tokio::main]`.

## A first function

The whole program: configuration read once at cold start, one handler, one pipeline.

```rust,no_run
use std::sync::Arc;

use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request};
use davidrs::{Context, RuntimeError};
use serde::{Deserialize, Serialize};

/// Everything the function builds once, before the first invocation.
struct App {
    greeting: String,
}

impl App {
    /// Reads configuration. A missing variable fails the cold start, not a request.
    fn from_env() -> Result<Self, RuntimeError> {
        Ok(Self {
            greeting: davidrs::optional_env("GREETING").unwrap_or_else(|| "Hello".to_owned()),
        })
    }
}

/// `GET /hello/{name}`: the path parameter, decoded.
#[derive(Deserialize)]
struct HelloPath {
    name: String,
}

#[derive(Serialize)]
struct Greeting {
    message: String,
}

async fn hello(app: Arc<App>, input: HelloPath, _context: Context<()>) -> Result<Json<Greeting>, Failure> {
    Ok(Json(Greeting {
        message: format!("{}, {}", app.greeting, input.name),
    }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("hello")?;
    let app = Arc::new(App::from_env()?);
    Api::new("hello", Public, PlainErrors)
        .run(app, |request: &Request| request.path::<HelloPath>(), hello)
        .await
}
```

What each part does:

- [`Api::new`](crate::http::Api::new) takes an operation name (used in logs and traces), a [`Policy`](crate::http::Policy) that decides who may call — [`Public`](crate::http::Public) here — and an [`ErrorRenderer`](crate::http::ErrorRenderer) that decides how a failure looks on the wire.
- The decoder, `|request| request.path::<HelloPath>()`, turns the request into the handler's input. It is synchronous and bounded; a malformed request becomes a `400` without the handler running.
- The handler returns [`Json`](crate::http::Json) on success and a [`Failure`](crate::http::Failure) otherwise.
- [`telemetry::init`](crate::telemetry::init) installs logging (and traces, with `otel`); keep the guard alive until `main` returns.

## Choosing features

Start from what triggers the function, then add what the handler calls:

| The function… | Features |
| --- | --- |
| answers API Gateway or a Function URL with one response | `http` |
| answers through a response stream (JSON or server-sent events) | `http-stream` |
| also receives REST API events | `apigw-rest` |
| consumes an SQS queue | `queue` (+ `queue-visibility` to delay a retry) |
| is an EventBridge target, or runs on a schedule | `event`, `schedule` |
| is invoked directly with a JSON payload | `runtime` |
| reads DynamoDB, publishes events, reads secrets | `aws` + `dynamo`, `events`, `secrets` |
| calls other HTTP services | `client` |
| verifies bearer tokens itself | `auth` |
| logs, emits metrics, traces to X-Ray | `logs`, `metrics`, `otel` |

The [features](crate::guide::features) chapter lists exactly what each one links.

In a Cargo workspace, features are unified across every member being built, which can hide a missing one. Check each function alone — `cargo check -p <function>` — the way a per-function build (`cargo lambda build --package <function>`) compiles it.

## Writing handlers

Every trigger uses the same handler shape:

```text
async fn(Arc<App>, Input, Context<Scope>) -> Result<Output, Error>
```

- `App` is built once in `main` and shared: clients, table names, configuration read with [`required_env`](crate::required_env).
- `Input` is what the decoder produced (HTTP) or the typed payload (every other trigger).
- [`Context`](crate::Context) carries the invocation — its request id, trace id and [`Deadline`](crate::Deadline) — and the scope the policy established.
- Build a 5xx with [`Failure::internal`](crate::http::Failure::internal) or [`Failure::from_error`](crate::http::Failure::from_error): the detail is kept for diagnostics and never rendered.

## Running locally

[Cargo Lambda](https://www.cargo-lambda.info) runs functions against a local Lambda emulator:

```bash
cargo lambda watch                                   # serves every binary on :9000
cargo lambda invoke hello --data-example apigw-request
```

The emulator sends a relative deadline instead of an epoch timestamp; the crate reads it as a budget from now, so deadlines behave as they do on Lambda.

The repository's `examples/` directory has one runnable program per trigger. None of them needs an AWS account except `table`, which reads a real DynamoDB table.

## Deploying

Build with `cargo lambda build --release --arm64` and deploy with your infrastructure tool (CDK, SAM, Terraform). Give each function a least-privilege role and set the environment variables its `App` reads.
