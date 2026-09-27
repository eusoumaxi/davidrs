# Introduction

`davidrs` is a toolkit for writing AWS Lambda functions in Rust that are fast, small and correct by default, so the code you write is the business logic and nothing else.

It is a thin layer — a wrapper over the official wrappers. AWS maintains [`lambda_runtime`](https://docs.rs/lambda_runtime) and [`lambda_http`](https://docs.rs/lambda_http), which speak the Lambda Runtime API, and the AWS SDK for Rust, which speaks to the services. `davidrs` sits on top of them and owns the part every function otherwise writes by hand around them: deadlines, the order of the request pipeline, error rendering that cannot leak, bounded reads, partial-batch reporting, rate limits, access control and telemetry.

```text
your handler            business logic: one async function per operation
davidrs                 pipelines, deadlines, failures, bounds, access, telemetry
lambda_runtime/_http    the official Rust runtime for Lambda (AWS)
aws-sdk-*               the official AWS SDK for Rust (AWS)
Lambda Runtime API      the platform
```

## Why it exists

`davidrs` was not designed on a whiteboard. It was extracted from a production API while its endpoints moved, one at a time, from Node.js Lambda functions to Rust. Every port repeated the same work, and every copy got a detail slightly wrong:

- a timeout measured from "now" inside each helper, so a retry got a fresh allowance and Lambda killed the function halfway through a write;
- a `500` built from an SDK error message, which put a table name or an upstream provider's text in front of the client;
- an SQS handler that returned `Err` and made Lambda redeliver the whole batch, the good messages included;
- `BatchWriteItem` "succeeding" while it quietly left items unwritten;
- a paginated read that stopped at a limit and returned a partial list as if it were complete;
- a streamed response whose producer task kept calling upstream services after the client had gone;
- tracing through a Lambda layer that added half a second to every cold start.

After the fifth copy of the same fix, the fixes became a crate. Each one is now a rule the crate enforces by construction, so a handler cannot forget it.

## What it looks like in production

[Nicer](https://nicer.travel), a travel platform, runs its production API on `davidrs`: public search, bookings, itineraries, customer records and an MCP server for AI assistants, across multiple upstream travel-content providers, serving millions of requests. Measured on the same endpoints before and after moving them from Node.js to Rust on `davidrs`, on arm64:

|  | Node.js | Rust on `davidrs` |
| --- | --- | --- |
| Cold start (Lambda `Init Duration`) | 800–1,300 ms | 60–120 ms |
| Warm invocation, no upstream call | ~17 ms | ~5 ms |
| Memory used | 175–300 MB | 20–75 MB |
| Handler code, after the shared plumbing moved into the crate | ~2,000 lines | ~340 lines |

The time left in a warm invocation is the upstream call itself: the framework's own overhead is measured in microseconds.

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

Everything around that function — who may call it, what happens when the body is malformed, how a failure looks on the wire, what is logged, when the deadline hits — is configured once in `main` and enforced by the crate. That is what makes the code small enough for a person to review in a minute, and predictable enough for an AI coding agent to write correctly: the agent writes the business rule, and the rules it could get wrong are not in the handler at all. The repository's `AGENTS.md` gives agents the same rules as `CONTRIBUTING.md` gives people.

## A wrapper, not a Lambda on top of a Lambda

Some ways of running Rust on Lambda add a second layer at run time:

- a web framework served inside the function behind an adapter, so every request makes an extra local HTTP hop into a second server;
- a proxy or "backend for frontend" function in front of the real one, which doubles the invocations, the cold starts and the bill;
- a generic router that serves many operations from one function, so every operation pays for the dependencies and the permissions of all the others.

`davidrs` adds structure at compile time and nothing at run time. There is one process, the official runtime, and your handler. One function serves one operation, with exactly the features, dependencies and IAM permissions that operation needs — which is why its deployment package stays small and its cold start short.

## Principles

- **One function, one operation.** No router, no middleware stack, no dependency-injection container, no ORM. State is a plain struct shared as `Arc<App>`; persistence uses the SDK's own builders.
- **Native types.** Requests are `lambda_http` requests, responses are `http` responses, AWS calls are the SDK's. Nothing hides the platform.
- **Pay for what you use.** Default features are empty. Each capability is a feature that links only what it names, so a function's `Cargo.toml` reads as its build.
- **Configurable where applications differ.** Who may call, what is admitted, how an error looks on the wire: each is a trait or a configurable policy ([`Access`](crate::http::access::Access), [`RateLimited`](crate::http::RateLimited), [`ErrorRenderer`](crate::http::ErrorRenderer)). Everything else is concrete.
- **Honest outcomes.** A partial result says it is partial.

## When not to use it

- For a long-running server (ECS, EC2, Kubernetes): use a web framework such as `axum`. This crate is built around Lambda's one-invocation-at-a-time model and its deadline.
- When one function should serve many routes: that is a router's job. Deploy one function per operation instead, or put a router in front of the pipeline yourself.
- When the function needs the full AWS credential chain (profiles, SSO, instance metadata): [`aws::sdk_config`](crate::aws::sdk_config) reads only what Lambda injects.

## Where to go next

1. [Getting started](crate::guide::getting_started): a first function, the features to enable, running it locally.
2. [Deployment](crate::guide::deployment): building with Cargo Lambda, sizes, and how a function connects to API Gateway, Function URLs and CloudFront.
3. [Architecture](crate::guide::architecture): how an invocation flows through each pipeline.
4. The chapter for each capability you use, listed in the [guide](crate::guide).
