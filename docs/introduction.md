# Introduction

`davidrs` is a small framework for AWS Lambda functions in Rust. It gives each trigger an explicit pipeline and reusable safeguards, so handlers can focus on business logic.

It is a thin layer over the official libraries. AWS maintains [`lambda_runtime`](https://docs.rs/lambda_runtime) and [`lambda_http`](https://docs.rs/lambda_http), which speak the Lambda Runtime API, and the AWS SDK for Rust, which speaks to the services. `davidrs` sits on top of them and owns the part every function otherwise writes by hand around them: deadlines, the order of the request pipeline, built-in error rendering with fixed 5xx messages, bounded reads, partial-batch reporting, rate limits, access control and telemetry.

```text
your handler            business logic: one async function per operation
davidrs                 pipelines, deadlines, failures, bounds, access, telemetry
lambda_runtime/_http    the official Rust runtime for Lambda (AWS)
aws-sdk-*               the official AWS SDK for Rust (AWS)
Lambda Runtime API      the platform
```

## Where to read next

| You want to | Read |
| --- | --- |
| Build one function and see the JSON it returns | [Getting started](crate::guide::getting_started) |
| Do one job: a queue, a query, a token check, an MCP server | That chapter, from the [guide index](crate::guide) |
| Look up a type or a function | The API reference on this site. Each page says what the item does, what it returns, and when it fails. The examples there are the same code the guide walks through. |
| Copy a whole program | `examples/` in the repository |

You do not have to read the guide in order after the tutorial. Rust samples are checked as doctests; `no_run` examples are compiled without execution.

## Why it exists

Most of a Lambda function is the code around its handler, and every function writes that code again. Each copy tends to get a detail slightly wrong:

- a timeout measured from "now" inside each helper, so a retry gets a fresh allowance and Lambda stops the function halfway through a write;
- a `500` built from an SDK error message, which puts a table name or an upstream service's text in front of the client;
- an SQS handler that returns `Err` and makes Lambda redeliver the whole batch, the good messages included;
- `BatchWriteItem` "succeeding" while it quietly leaves items unwritten;
- a paginated read that stops at a limit and returns a partial list as if it were complete;
- a streamed response whose producer task keeps calling upstream services after the client has gone;
- telemetry setup that adds an exporter and a background task whose lifetime the function does not control.

`davidrs` centralizes this runtime work. Application permissions, service configuration and business rules remain explicit extension points.

## Performance

Default features are empty, so each binary includes only the capabilities it
uses. The framework runs in the function process and adds no local HTTP
server. Package size and latency still depend on the compiler, enabled
features, memory setting, initialization and upstream services.

This repository does not include a reproducible benchmark suite. Measure cold
and warm invocations separately with your own workload; the
[deployment guide](crate::guide::deployment#measuring-a-function) explains what
to record.

## What you write

A handler is an ordinary async function. You do not write the pipeline, the deadline or the error envelope. It receives the application's shared state, its decoded input and a [`Context`](crate::Context), and returns a value or a failure:

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

The pipeline's policies, error renderer and deadline are configured in `main`.
The handler owns the business rule and must use the invocation deadline to
bound its own I/O. The `davidrs` agent skill (`npx skills add eusoumaxi/davidrs`)
teaches coding agents the patterns in this guide; their output still needs
review and tests. The repository's `AGENTS.md` gives agents working on the
crate itself the same rules as `CONTRIBUTING.md` gives people.

## One process, the official runtime, your handler

`davidrs` runs the pipeline in the same process as the handler through the
official Lambda runtime. It does not start a local HTTP server or add a proxy
function. One function serves one operation, so its Cargo features and IAM
permissions can be chosen for that operation. Package size, cold-start latency
and cost depend on the workload and deployment; measure them as described in
the [deployment guide](crate::guide::deployment#measuring-a-function).

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
| API Gateway, Function URL, ALB, WebSocket and VPC Lattice events as `http` requests (`lambda_http`) | Cross-compilation with Zig, release defaults and zip output (`build`) | Ordered HTTP pipelines with one error renderer and fixed public 5xx messages |
| Typed payloads for S3, SNS, Kinesis and DynamoDB streams, Cognito and more (`aws_lambda_events`) | Quick manual deploys (`deploy`) | Bounded bodies, reads and retries; honest SQS, EventBridge and DynamoDB partial outcomes; access control, rate limits, MCP servers, X-Ray export |

**Where the official crates go further.** Use them directly, next to `davidrs`, when a function needs:

- **A trigger without a pipeline here** — S3, SNS, Kinesis or DynamoDB streams, Cognito triggers: [`runtime::run`](crate::runtime::run) accepts their `aws_lambda_events` payloads, as shown below.
- **WebSocket or VPC Lattice events.** `davidrs` reads API Gateway, Function URL and, with the `alb` feature, Application Load Balancer payloads of `lambda_http`.
- **Concurrent invocations, tower layers or SnapStart hooks.** `lambda_runtime` runs several invocations at once on Lambda Managed Instances and accepts tower layers; the `davidrs` entry points run one invocation at a time and take no layers of yours.
- **A JSON log format.** `lambda_runtime`'s default subscriber follows `AWS_LAMBDA_LOG_FORMAT`; [`telemetry::init`](crate::telemetry::init) follows `AWS_LAMBDA_LOG_LEVEL` but always writes plain text.

What these crates offer elsewhere, `davidrs` covers in its own terms: form bodies with [`Request::form`](crate::http::Request::form), and the `errorType` Lambda records — the name a Step Functions `Retry` or `Catch` matches — through the official [`Diagnostic`](crate::runtime::Diagnostic) type that every non-HTTP handler's error converts into.

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

1. [Getting started](crate::guide::getting_started): create the function, run it, and read the success and error bodies.
2. The chapter for the trigger you are deploying, from the table on the [guide index](crate::guide).
3. [Deployment](crate::guide::deployment): the binary, and the AWS setting that makes a correct partial-failure report do nothing if you forget it.
4. [Architecture](crate::guide::architecture): the order of steps, once you need to know where a policy or a finalizer runs.
