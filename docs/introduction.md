# Introduction

`davidrs` is a framework for writing AWS Lambda functions in Rust that are fast, small and correct by default, so the code you write is the business logic and nothing else.

It is a thin layer over the official libraries. AWS maintains [`lambda_runtime`](https://docs.rs/lambda_runtime) and [`lambda_http`](https://docs.rs/lambda_http), which speak the Lambda Runtime API, and the AWS SDK for Rust, which speaks to the services. `davidrs` sits on top of them and owns the part every function otherwise writes by hand around them: deadlines, the order of the request pipeline, error rendering that cannot leak, bounded reads, partial-batch reporting, rate limits, access control and telemetry.

```text
your handler            business logic: one async function per operation
davidrs                 pipelines, deadlines, failures, bounds, access, telemetry
lambda_runtime/_http    the official Rust runtime for Lambda (AWS)
aws-sdk-*               the official AWS SDK for Rust (AWS)
Lambda Runtime API      the platform
```

## Why it exists

Most of a Lambda function is the code around its handler, and every function writes that code again. Each copy tends to get a detail slightly wrong:

- a timeout measured from "now" inside each helper, so a retry gets a fresh allowance and Lambda stops the function halfway through a write;
- a `500` built from an SDK error message, which puts a table name or an upstream service's text in front of the client;
- an SQS handler that returns `Err` and makes Lambda redeliver the whole batch, the good messages included;
- `BatchWriteItem` "succeeding" while it quietly leaves items unwritten;
- a paginated read that stops at a limit and returns a partial list as if it were complete;
- a streamed response whose producer task keeps calling upstream services after the client has gone;
- tracing through a Lambda layer that adds half a second to every cold start.

`davidrs` turns each of these into a rule it enforces by construction, so a handler cannot forget it.

## Measured results

Measured on a production API, comparing the same endpoints before and after they moved from Node.js Lambda functions to Rust on `davidrs`, on arm64:

|  | Node.js | Rust on `davidrs` |
| --- | --- | --- |
| Cold start (Lambda `Init Duration`) | 800–1,300 ms | 60–120 ms |
| Warm invocation, no upstream call | ~17 ms | ~5 ms |
| Memory used | 175–300 MB | 20–75 MB |
| Handler code, after the shared plumbing moved into the crate | ~2,000 lines | ~340 lines |

In a warm invocation that calls an upstream service, nearly all of the time is that call: the framework's own overhead is measured in microseconds.

## Built for business logic — yours and your AI's

A handler is an ordinary async function. It receives the application's shared state, its decoded input and a [`Context`](crate::Context), and returns a value or a failure:

```rust
use std::sync::Arc;

use davidrs::http::{Failure, Json};
use davidrs::Context;
use serde::{Deserialize, Serialize};

/// What the function builds once, at cold start.
struct App {
    greeting: String,
}

/// The decoded request.
#[derive(Deserialize)]
struct Hello {
    name: String,
}

/// The response body.
#[derive(Serialize)]
struct Greeting {
    message: String,
}

async fn hello(app: Arc<App>, input: Hello, _context: Context<()>) -> Result<Json<Greeting>, Failure> {
    Ok(Json(Greeting {
        message: format!("{}, {}", app.greeting, input.name),
    }))
}
```

Everything around that function — who may call it, what happens when the body is malformed, how a failure looks on the wire, what is logged, when the deadline hits — is configured once in `main` and enforced by the crate. That is what makes the code small enough for a person to review in a minute, and predictable enough for an AI coding agent to write correctly: the agent writes the business rule, and the rules it could get wrong are not in the handler at all. The `davidrs` agent skill (`npx skills add eusoumaxi/davidrs`) teaches coding agents to write functions the way this guide does, and the repository's `AGENTS.md` gives agents working on the crate itself the same rules as `CONTRIBUTING.md` gives people.

## A wrapper, not a Lambda on top of a Lambda

Some ways of running Rust on Lambda add a second layer at run time:

- a web framework served inside the function behind an adapter, so every request makes an extra local HTTP hop into a second server;
- a proxy or "backend for frontend" function in front of the real one, which doubles the invocations, the cold starts and the bill;
- a generic router that serves many operations from one function, so every operation pays for the dependencies and the permissions of all the others.

`davidrs` adds structure at compile time and nothing at run time. There is one process, the official runtime, and your handler. One function serves one operation, with exactly the features, dependencies and IAM permissions that operation needs — which is why its deployment package stays small and its cold start short.

## Principles

- **One function, one operation.** No router, no middleware stack, no dependency-injection container, no ORM. State is a plain struct shared as `Arc<App>`; persistence uses the SDK's own builders.
- **Native types.** Requests are `lambda_http` requests, responses are `http` responses, AWS calls are the SDK's. Decoders and policies reach the native request through [`Request::native`](crate::http::Request::native); handlers receive a [`Context`](crate::Context) with what every trigger shares, instead of `lambda_runtime`'s own.
- **Pay for what you use.** Default features are empty. Each capability is a feature that links only what it names, so a function's `Cargo.toml` reads as its build.
- **Configurable where applications differ.** Who may call, what is admitted, how an error looks on the wire: each is a trait or a configurable policy ([`Access`](crate::http::access::Access), [`RateLimited`](crate::http::RateLimited), [`ErrorRenderer`](crate::http::ErrorRenderer)). Everything else is concrete.
- **Honest outcomes.** A partial result says it is partial.

## How it relates to the official runtime and Cargo Lambda

`davidrs` builds on AWS's runtime crates instead of replacing them, and works with Cargo Lambda rather than beside it. Every entry point builds a `lambda_runtime::Runtime` and runs it; the HTTP pipelines read requests with `lambda_http`, whose `http` types [`davidrs::http`](crate::http) re-exports unchanged. Cargo Lambda is a command-line tool, not a dependency: it builds the `bootstrap` binary and emulates Lambda on your machine.

| The official crates provide | Cargo Lambda provides | `davidrs` adds |
| --- | --- | --- |
| The Runtime API loop and payload deserialization (`lambda_runtime`) | A local emulator of the Runtime API and of Function URLs (`watch`, `invoke`) | One enforced, monotonic deadline per invocation |
| API Gateway, Function URL, ALB, WebSocket and VPC Lattice events as `http` requests (`lambda_http`) | Cross-compilation with Zig, release defaults and zip output (`build`) | Ordered HTTP pipelines with one error renderer and 5xx messages that cannot leak |
| Typed payloads for S3, SNS, Kinesis and DynamoDB streams, Cognito and more (`aws_lambda_events`) | Quick manual deploys (`deploy`) | Bounded bodies, reads and retries; honest SQS, EventBridge and DynamoDB partial outcomes; access control, rate limits, MCP servers, X-Ray export |

**Where the official crates go further.** Use them directly, next to `davidrs`, when a function needs:

- **A trigger without a pipeline here** — S3, SNS, Kinesis or DynamoDB streams, Cognito triggers: [`runtime::run`](crate::runtime::run) accepts their `aws_lambda_events` payloads, as shown below.
- **ALB, WebSocket or VPC Lattice events.** `davidrs` enables only the API Gateway and Function URL payloads of `lambda_http`.
- **Concurrent invocations, tower layers or SnapStart hooks.** `lambda_runtime` runs several invocations at once on Lambda Managed Instances and accepts tower layers; the `davidrs` entry points run one invocation at a time, with no layers.
- **Lambda's advanced logging controls.** `lambda_runtime`'s default subscriber honours `AWS_LAMBDA_LOG_LEVEL` and `AWS_LAMBDA_LOG_FORMAT`; [`telemetry::init`](crate::telemetry::init) reads `RUST_LOG` only.
- **Form bodies.** `lambda_http`'s `RequestPayloadExt` parses `application/x-www-form-urlencoded`; a decoder reaches it through [`Request::native`](crate::http::Request::native).
- **A chosen error type.** A non-HTTP handler's error reaches Lambda with its `Display` text and a Rust type name as `errorType`; a plain `lambda_runtime` handler chooses both through its own `Diagnostic`, which matters when Step Functions matches errors by name.

**Where `davidrs` goes further.** The official crates hand a handler an epoch deadline that nothing enforces, decode JSON with no size limit of their own, turn a failed HTTP handler into an invocation error — a gateway `502` with no body — instead of a response, leave SQS partial batches and streamed producers to each function, and include no trace exporter. Those are the parts this crate owns.

**`aws_lambda_events` payloads.** For a trigger with no pipeline here, give [`runtime::run`](crate::runtime::run) a type from the `aws_lambda_events` crate: it accepts any payload that implements `DeserializeOwned` and any response that implements `Serialize`. Depend on version 1, the one `lambda_http` uses, with only the events you need:

```toml
aws_lambda_events = { version = "1", default-features = false, features = ["s3"] }
```

A Kinesis or DynamoDB stream handler can return `KinesisEventResponse` or `DynamoDbEventResponse` to report partial batch failures, provided the event source mapping enables `ReportBatchItemFailures`.

**Local runs.** `cargo lambda watch` serves binary crates only, sends every invocation a relative deadline of `600000` that the crate reads as ten minutes from now, and adds permissive CORS headers unless it runs with `--disable-cors`. [Getting started](crate::guide::getting_started) shows a local run.

## When not to use it

- For a long-running server (ECS, EC2, Kubernetes): use a web framework such as `axum`. This crate is built around Lambda's one-invocation-at-a-time model and its deadline.
- When one function should serve many routes: that is a router's job. Deploy one function per operation instead, or put a router in front of the pipeline yourself.
- When the function needs the full AWS credential chain (profiles, SSO, instance metadata): [`aws::sdk_config`](crate::aws::sdk_config) reads only what Lambda injects.

## Where to go next

1. [Getting started](crate::guide::getting_started): a first function, the features to enable, running it locally.
2. [Deployment](crate::guide::deployment): building with Cargo Lambda, sizes, and how a function connects to API Gateway, Function URLs and CloudFront.
3. [Architecture](crate::guide::architecture): how an invocation flows through each pipeline.
4. The chapter for each capability you use, listed in the [guide](crate::guide).
