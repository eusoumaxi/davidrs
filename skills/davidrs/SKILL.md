---
name: davidrs
description: Build AWS Lambda functions in Rust with the davidrs crate. HTTP endpoints behind API Gateway or Function URLs, SQS consumers with partial batch failures, EventBridge rules and schedules, direct invocations and other event sources (S3, DynamoDB Streams, Kinesis, SNS), DynamoDB, EventBridge publishing, Secrets Manager, outbound HTTP, and MCP servers for AI clients. Use when creating, changing, testing or deploying a Lambda function, or a Cargo workspace of functions, that uses davidrs; when choosing its features, writing Cargo.toml and main.rs, handlers, Failure or RuntimeError, deadlines and tests without an AWS account; or when running it locally and building it for arm64 with Cargo Lambda.
license: MIT
---

# davidrs

`davidrs` is a small framework over the official `lambda_runtime` and `lambda_http` crates and the AWS SDK. One function serves one operation: `main` reads configuration once, builds the application state and hands one handler to one pipeline. There is no router, middleware stack, dependency-injection container or ORM, and none should be added. Use this skill for any Rust Lambda function that depends on `davidrs`, and read the reference for its trigger or service (see References) before writing code.

## Workflow

1. Choose the pipeline and the features from the table below; enable nothing else. Use Rust 1.98.1 or later.
2. Write `App` and `App::from_env`: every setting read once with `required_env` / `optional_env`, every client built once.
3. Write the handler: `async fn(Arc<App>, Input, Context<Scope>) -> Result<Output, Error>`.
4. Wire `main`: telemetry guard, `Arc::new(App::from_env()?)`, one pipeline call.
5. Test the handler without AWS, then check the function alone: `cargo check -p <function>`, `cargo test -p <function>`.
6. Run it with `cargo lambda watch`, build it with `cargo lambda build --release --arm64`, deploy it with infrastructure as code.

| The function… | Features | `main` ends with |
| --- | --- | --- |
| answers API Gateway or a Function URL | `http` | `Api::new(op, policy, renderer).run(app, decode, handler)` |
| streams JSON or server-sent events over HTTP | `http-stream` | `StreamApi::new(op, policy, renderer).run(app, handler)` |
| consumes an SQS queue | `queue` (+ `queue-visibility`) | `davidrs::queue::run(app, handler)` |
| is an EventBridge rule target / runs on a schedule | `event` / `schedule` | `davidrs::event::run` / `davidrs::schedule::run` |
| is invoked directly, or by S3, DynamoDB Streams, Kinesis, SNS | `runtime` | `davidrs::runtime::run(app, handler)` |
| streams a response without the HTTP pipeline | `streaming` | `davidrs::streaming::run(app, handler)` |
| serves MCP tools to AI clients | `mcp` (+ `mcp-openapi`) | `mcp::Server::new(name, version, policy)` … `.run(app)` |

Then add only what the handler calls: `dynamo`, `eventbridge`, `secrets` (each enables `aws`), `client`, `auth`, `apigw-rest`, `validate`, `problem`, `logs`, `metrics`, `otel`, `cache`, `compression`, `digest`.

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["http", "logs"] }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

[dev-dependencies]
davidrs = { version = "0.1", default-features = false, features = ["test-support"] }
```

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request};
use davidrs::{required_env, Context, RuntimeError};
use serde::{Deserialize, Serialize};

/// Built once per cold start and shared by every invocation.
struct App {
    table: String,
}

impl App {
    fn from_env() -> Result<Self, RuntimeError> {
        Ok(Self {
            table: required_env("ORDERS_TABLE")?,
        })
    }
}

#[derive(Deserialize)]
struct NewOrder {
    item: String,
}

#[derive(Serialize)]
struct Created {
    id: String,
}

/// Stands in for a DynamoDB write (references/aws-services.md).
async fn save(table: &str, item: &str) -> Result<String, std::io::Error> {
    Ok(format!("{table}/{item}"))
}

async fn create(
    app: Arc<App>,
    input: NewOrder,
    ctx: Context<()>,
) -> Result<Json<Created>, Failure> {
    let id = ctx
        .deadline()
        .child(Duration::from_secs(3))
        .run(save(&app.table, &input.item))
        .await?
        .map_err(|error| Failure::from_error("FAULT_ORDER_STORE", &error))?;
    Ok(Json(Created { id }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders-create")?;
    let app = Arc::new(App::from_env()?);
    Api::new("orders-create", Public, PlainErrors)
        .run(app, |request: &Request| request.json::<NewOrder>(), create)
        .await
}
```

- **Errors.** HTTP handlers and MCP tools return `http::Failure`: `Failure::new(status, "ERROR_…", "message the client may read")` for a 4xx; `Failure::internal(code, detail)` or `Failure::from_error(code, &error)` for a 5xx, whose text is never rendered. `?` on a `RuntimeError` is a `500`. Other triggers return an `E: Display`, usually `RuntimeError::message(..)` or `RuntimeError::other("what was being done", error)`; that `Err` is what Lambda retries, so never swallow a failure into `Ok`.
- **Deadlines.** `ctx.deadline()` is one absolute budget; the pipeline keeps 100 ms (1 s for `StreamApi`) to answer. Bound every await that can stall with `deadline.run(..)`, derive budgets with `child(..)` and `with_margin(..)`, and give retries attempts from one parent, never a fresh timeout each.
- **Tests.** Call the handler with `Context::new(test_support::invocation("r-1"), ())` (`expired_invocation` for the deadline path), run the HTTP pipeline with `api.handle(app, test_support::post_json(..), &decode, &handler)`, and an SQS batch with `davidrs::queue::process`.
- **Local run.** Install with `brew install cargo-lambda/tap/cargo-lambda` or `pip3 install cargo-lambda`. `cargo lambda watch` serves binary crates, not `examples/`, on `:9000`. Map `curl` routes in the function's `Cargo.toml` under `[package.metadata.lambda.watch.router]`, such as `"/orders/{id}" = "orders-get"`; send other triggers a payload with `cargo lambda invoke <function> --data-file event.json`. The emulator always sends a relative deadline of 600000 ms (ten minutes, whatever the deployed timeout: test short deadlines with `test_support`) and adds permissive CORS unless started with `--disable-cors`.

## Invariants

- One function, one operation, one pipeline call. Application policy goes through `Policy`, `Admission`, `ErrorRenderer`, `finalize` and `prepare`, never a router or middleware.
- State is a plain struct built in `main` and shared as `Arc<App>`; never build a client or read the environment in a handler. Parse numbers yourself and report a bad value as `RuntimeError::Configuration`. Secrets come from Secrets Manager.
- Bounded work: body limits, page limits, bounded readers; partial outcomes reported as partial.
- Enable only the features a function uses and check it alone (`cargo check -p`): Cargo unifies features across a workspace, which hides a missing one. One `[profile.release]`, at the workspace root.
- Let the platform do what it can (API Gateway authorizers and throttling, AWS WAF, Cognito, SQS redrive, Lambda retries); the function does what only the application knows.
- Behind an API Gateway authorizer the token is already verified: do not verify it again, map its claims with `Access` (`gateway_token_claims(true)` for nested claims). Verify tokens (`verify_bearer`) only where no authorizer sits in front.
- No panics on a request path: the release profile aborts, which ends the sandbox.

## References

| Task | Read |
| --- | --- |
| The full function template, `Failure` vs `RuntimeError`, deadlines, tests, workspace layout, release profile, local runs, deployment | [references/functions.md](references/functions.md) |
| SQS, EventBridge, schedules, direct invocations, S3, DynamoDB Streams, Kinesis, SNS, streamed responses without the HTTP pipeline | [references/event-consumers.md](references/event-consumers.md) |
| API Gateway and Function URL endpoints, buffered or streamed: decoders, failures and renderers, access control, quotas, validation, partial responses, what API Gateway, AWS WAF, Cognito and CloudFront do first | [references/http.md](references/http.md) |
| SDK configuration, DynamoDB, EventBridge publishing, Secrets Manager, outbound HTTP, bearer tokens, caches, gzip, SHA-256, logs, metrics, traces, least-privilege IAM | [references/aws-services.md](references/aws-services.md) |
| MCP servers for AI clients: hand-written tools, tools from an OpenAPI document, API keys and OAuth | [references/mcp.md](references/mcp.md) |

The [guide](https://docs.rs/davidrs/latest/davidrs/guide/index.html) explains every capability; [examples/](https://github.com/eusoumaxi/davidrs/tree/main/examples) has a program for each trigger.
