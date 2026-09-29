# davidrs

[![crates.io](https://img.shields.io/crates/v/davidrs.svg)](https://crates.io/crates/davidrs)
[![docs.rs](https://img.shields.io/docsrs/davidrs)](https://docs.rs/davidrs)
[![CI and release](https://github.com/eusoumaxi/davidrs/actions/workflows/release.yml/badge.svg?branch=main)](https://github.com/eusoumaxi/davidrs/actions/workflows/release.yml)
[![MSRV](https://img.shields.io/crates/msrv/davidrs)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Build focused AWS Lambda functions in Rust. `davidrs` provides typed handlers,
invocation deadlines, bounded I/O and built-in error responses that keep
internal details out of HTTP 5xx bodies. It builds on the official AWS Lambda
runtime and AWS SDK, with one function per operation and empty default features.

[Getting started](https://eusoumaxi.github.io/davidrs/davidrs/guide/getting_started/index.html)
· [Guide](https://eusoumaxi.github.io/davidrs/davidrs/guide/index.html)
· [API reference](https://docs.rs/davidrs)
· [Examples](examples/README.md)
· [Architecture](https://eusoumaxi.github.io/davidrs/davidrs/guide/architecture/index.html)

## What it handles

- **HTTP and events.** Typed HTTP handlers, response streams, SQS consumers, EventBridge targets, schedules and direct invocations.
- **Time and resource limits.** One absolute invocation deadline, bounded bodies and reads, and explicit retry and pagination limits.
- **Errors and partial results.** Fixed public 5xx messages, per-record SQS failures, and per-entry DynamoDB and EventBridge outcomes.
- **AWS services and telemetry.** Helpers around the native SDK, structured metrics and X-Ray traces, with clients and state owned by the application.
- **MCP on Lambda.** Hand-written tools or tools generated from an OpenAPI document.

Enable only the capabilities a function uses. Application authorization,
admission and error formatting stay configurable through explicit extension
points. Shared state is an ordinary `Arc<App>`; service calls use the AWS SDK's
types and builders. The [introduction](https://eusoumaxi.github.io/davidrs/davidrs/guide/introduction/index.html)
explains where the crate fits and when to use another approach.

## Quick start

Use Rust **1.94.1 or later**. Create a binary crate:

```bash
cargo new hello --bin
cd hello
```

Replace its `[dependencies]` section in `Cargo.toml`:

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["http"] }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Replace `src/main.rs` with this complete function:

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

Check the program without an AWS account or credentials:

```bash
cargo check
```

With an API Gateway route `GET /hello/{name}`, a request to `/hello/world`
returns HTTP 200:

```json
{"message":"Hello, world"}
```

`Public` allows every caller. `PlainErrors` renders decoding and handler
failures as JSON; its HTTP 5xx responses use the fixed message
`InternalServerError`. The operation name `"hello"` identifies logs and traces;
the route and its path parameters are configured in API Gateway or the local
emulator.

Follow [Getting started](https://eusoumaxi.github.io/davidrs/davidrs/guide/getting_started/index.html)
to run this pattern locally with Cargo Lambda, add logging and inspect error
responses. Use [Deployment](https://eusoumaxi.github.io/davidrs/davidrs/guide/deployment/index.html)
for the build, IAM permissions and trigger settings.

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

The [skill](skills/davidrs/SKILL.md) covers feature selection, `main`, handlers,
deadlines, tests and Cargo Lambda. Its [references](skills/davidrs/references/)
provide examples for HTTP endpoints, event consumers, AWS services and MCP
servers.

## Security

[SECURITY.md](SECURITY.md) describes the threat model, the crate's guarantees
and the application's responsibilities. Configure authentication, IAM and
traffic protection for your deployment; the
[AWS security guide](https://eusoumaxi.github.io/davidrs/davidrs/guide/aws_security/index.html)
shows how these fit together. Report suspected vulnerabilities through
[private vulnerability reporting](https://github.com/eusoumaxi/davidrs/security/advisories/new).

## Contributing

Contributions are welcome, including AI-assisted ones. [CONTRIBUTING.md](CONTRIBUTING.md) explains the design rules and the branch, commit and pull request conventions; [SECURITY.md](SECURITY.md) explains how to report a vulnerability privately. Every change is recorded in [CHANGELOG.md](CHANGELOG.md).

Use the [issue forms](https://github.com/eusoumaxi/davidrs/issues/new/choose) for
bug reports and feature requests. Published versions and their release notes
are on [GitHub Releases](https://github.com/eusoumaxi/davidrs/releases).

```bash
scripts/check.sh                      # everything CI runs
git config core.hooksPath .githooks   # check commit messages locally
```

## Licence

`davidrs` is created by David Lara and released under the [MIT licence](LICENSE): use it, change it and ship it, commercially or not, keeping the copyright notice.
