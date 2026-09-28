# davidrs

[![crates.io](https://img.shields.io/crates/v/davidrs.svg)](https://crates.io/crates/davidrs)
[![docs.rs](https://img.shields.io/docsrs/davidrs)](https://docs.rs/davidrs)
[![CI](https://github.com/eusoumaxi/davidrs/actions/workflows/ci.yml/badge.svg)](https://github.com/eusoumaxi/davidrs/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/crates/msrv/davidrs)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A small, feature-gated framework for AWS Lambda functions in Rust. You write the business logic. The crate owns the surrounding code that is easy to get subtly wrong: deadlines, the order of an HTTP request, error bodies that must not leak, and partial failures that must not be reported as success.

**[Guide and API reference →](https://eusoumaxi.github.io/davidrs/)**

Start with the [introduction](https://eusoumaxi.github.io/davidrs/davidrs/guide/introduction/index.html) if you want to know whether the crate fits, then the [getting started](https://eusoumaxi.github.io/davidrs/davidrs/guide/getting_started/index.html) tutorial, which builds one function and shows the JSON a caller receives. The [guide index](https://eusoumaxi.github.io/davidrs/davidrs/guide/index.html) is organized by the job in front of you (an HTTP route, an SQS consumer, a DynamoDB page, an MCP server), not by module name.

`davidrs` keeps the official `lambda_runtime` / `lambda_http` adapters and the AWS SDK as the transport, and owns what every function ends up writing around them:

- **One absolute deadline per invocation.** Child budgets can only shrink, retries do not get a fresh allowance, and every adapter keeps a margin for cleanup before Lambda's own timeout.
- **One ordered pipeline per trigger.** Buffered HTTP runs admission, decoding, authorization and the handler in order. Streamed HTTP runs preparation, CORS, negotiation, authorization and the handler. Both use one error renderer before the response starts. SQS, EventBridge, schedules and direct invocations decode their payload before the handler and report its failures to Lambda.
- **Safe error rendering.** A failure keeps its public message and internal detail apart. The built-in renderers use a fixed message for every 5xx.
- **Explicit limits.** Buffered request bodies, bounded upstream readers, decompressed payloads, paginated reads, retries and stream producers have limits and report when they hit one.
- **Honest partial outcomes.** SQS batches report failures per record; `PutEvents`, `BatchWriteItem` and `BatchGetItem` per entry; a bounded read says whether it is complete.

It is deliberately **not** a web framework: one Lambda serves one operation,
so there is no router, no middleware stack, no dependency-injection container
and no ORM. Enable only the features each function needs. Measure package
size, memory and latency with your own workload and deployment settings.

Run it behind API Gateway with AWS WAF and Amazon Cognito: the platform verifies tokens, throttles and filters before the function runs, and the crate covers what only the application knows — the [security chapter](https://eusoumaxi.github.io/davidrs/davidrs/guide/aws_security/index.html) explains the split.

The handlers you write are the business logic and nothing else — small enough
for a person to review at a glance, and predictable enough for an AI coding
agent to write correctly.

## Example

```rust,no_run
use std::sync::Arc;

use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request};
use davidrs::{Context, RuntimeError};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct HelloPath {
    name: String,
}

#[derive(Serialize)]
struct Greeting {
    message: String,
}

async fn hello(_app: Arc<()>, input: HelloPath, _context: Context<()>) -> Result<Json<Greeting>, Failure> {
    Ok(Json(Greeting { message: format!("Hello, {}", input.name) }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    Api::new("hello", Public, PlainErrors)
        .run(Arc::new(()), |request: &Request| request.path::<HelloPath>(), hello)
        .await
}
```

`GET /hello/world` is HTTP 200 and `{"message":"Hello, world"}`. A path that does not match `HelloPath` never calls `hello`: the caller receives HTTP 400 and `{"errorCode":"ERROR_INVALID_PATH","errorMessage":"Invalid request path"}`. A server failure is HTTP 500 with `"errorMessage":"InternalServerError"`, whatever detail you attached.

What each piece is doing:

- `Api::new` configures the pipeline once. `"hello"` is the operation name in logs and traces, not the URL. [`Public`](https://docs.rs/davidrs/latest/davidrs/http/struct.Public.html) admits every caller. [`PlainErrors`](https://docs.rs/davidrs/latest/davidrs/http/struct.PlainErrors.html) writes the `errorCode` / `errorMessage` JSON above.
- The closure reads the path parameter into `HelloPath` before the handler runs. A JSON body uses `request.json::<T>()` the same way. Decoding is synchronous and size-limited.
- `hello` receives shared state (`Arc<()>` here, your own struct in a real function), the decoded input, and a [`Context`](https://docs.rs/davidrs/latest/davidrs/struct.Context.html) with the request id and the deadline. It returns [`Json`](https://docs.rs/davidrs/latest/davidrs/http/struct.Json.html) or a [`Failure`](https://docs.rs/davidrs/latest/davidrs/http/struct.Failure.html).
- `run` starts the Lambda loop. It returns only when the runtime itself fails. A bad request is a response, not an error from `main`.

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["http"] }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Default features are empty. Name every capability the function uses. `http` is enough for the example; add `logs` when you want the log lines the tutorial turns on.

## Features

Default features are empty; each capability links only what it names.

| Feature | Enables |
| --- | --- |
| `runtime` | the native Lambda loop over any typed payload |
| `http` | the buffered API Gateway / Function URL pipeline, access control, rate limiting, partial responses |
| `http-stream` | streamed JSON and server-sent events, with CORS and content negotiation |
| `apigw-rest` | REST API events next to HTTP API |
| `alb` | Application Load Balancer events in the buffered pipeline |
| `streaming` | response bodies that own their producer |
| `queue`, `queue-visibility` | SQS partial batches, visibility changes |
| `event`, `schedule` | typed EventBridge and scheduled payloads |
| `aws` | SDK configuration from the Lambda environment |
| `dynamo` | bounded DynamoDB reads and batches, page tokens, spans |
| `eventbridge` | EventBridge publishing with per-entry outcomes |
| `secrets` | typed Secrets Manager reads that never echo a value |
| `client` | outbound HTTP with byte and time limits |
| `auth` | RS256 / JWKS bearer-token verification |
| `cache`, `compression`, `digest` | in-process caches, bounded gzip, SHA-256 |
| `validate`, `problem` | Garde validation, RFC 9457 problem details |
| `logs`, `metrics`, `otel` | logs, CloudWatch EMF metrics, X-Ray traces |
| `mcp`, `mcp-openapi` | MCP servers for AI clients: hand-written tools, or tools from an OpenAPI document |
| `test-support` | synthetic invocations and requests for tests |

## Documentation

The documentation is split by the question you have, so a reference page is not where you learn the crate and a tutorial is not where you look up a type.

| Question | Where |
| --- | --- |
| Does this crate fit, and what does a first function look like? | [Introduction](https://eusoumaxi.github.io/davidrs/davidrs/guide/introduction/index.html) and [Getting started](https://eusoumaxi.github.io/davidrs/davidrs/guide/getting_started/index.html) |
| How do I do this job (HTTP, SQS, DynamoDB, tokens, MCP, …)? | The matching chapter from the [guide index](https://eusoumaxi.github.io/davidrs/davidrs/guide/index.html). It shows the working path, what the caller or Lambda should observe, and what to change when that is not what you see. |
| What does this type or function do? | The API reference on the same site. Every public item has rustdoc. |
| Can I copy a whole program? | [`examples/`](examples/), one program per trigger. [`examples/README.md`](examples/README.md) says how to compile and run each one. |
| What does the crate guarantee, and what do I still have to decide? | [SECURITY.md](SECURITY.md) |
| How do the modules and extension points fit together? | [Architecture](https://eusoumaxi.github.io/davidrs/davidrs/guide/architecture/index.html) |
| How do I change the crate? | [CONTRIBUTING.md](CONTRIBUTING.md) |

Rust samples in the guide and API reference are checked as doctests. Samples
marked `no_run` are compiled without contacting AWS or starting a runtime loop.

## Skill for AI coding agents

The repository ships an [agent skill](https://github.com/vercel-labs/skills) that teaches coding agents (Claude Code, Cursor, Codex, Copilot and others) to write functions with `davidrs` the way the guide does:

```bash
npx skills add eusoumaxi/davidrs
```

`skills/davidrs/SKILL.md` covers every function: features, `main`, handlers, deadlines, tests and Cargo Lambda. The agent reads the reference it needs from `skills/davidrs/references/`: HTTP endpoints, event consumers, AWS services and MCP servers.

## Contributing

Contributions are welcome, including AI-assisted ones. [CONTRIBUTING.md](CONTRIBUTING.md) explains the design rules and the branch, commit and pull request conventions; [SECURITY.md](SECURITY.md) explains how to report a vulnerability privately. Every change is recorded in [CHANGELOG.md](CHANGELOG.md).

```bash
scripts/check.sh                      # everything CI runs
git config core.hooksPath .githooks   # check branch names and commit messages locally
```

## Licence

`davidrs` is created by David Lara and released under the [MIT licence](LICENSE): use it, change it and ship it, commercially or not, keeping the copyright notice.
