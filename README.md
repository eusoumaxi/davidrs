# davidrs

A small, feature-gated framework for AWS Lambda functions in Rust.

**[Guide and API reference →](https://eusoumaxi.github.io/davidrs/)**

`davidrs` keeps the official `lambda_runtime` / `lambda_http` adapters and the AWS SDK as the transport, and owns what every function ends up writing around them — the part that is easy to get subtly wrong:

- **One absolute deadline per invocation.** Child budgets can only shrink, retries do not get a fresh allowance, and every adapter keeps a margin for cleanup before Lambda's own timeout.
- **One ordered pipeline per trigger.** HTTP (buffered and streamed), SQS, EventBridge, schedules and direct invocations run admission, decoding, authorization and the handler in a fixed order, and every failure — including the serialization of a success — reaches one renderer.
- **Failures that cannot leak.** A failure keeps its public message and its internal detail apart, and every 5xx renders a fixed message however it was built.
- **Bounded work everywhere.** Request bodies, upstream responses, decompressed payloads, paginated reads, retries and stream producers all have explicit limits, and say when they hit one.
- **Honest partial outcomes.** SQS batches report failures per record; `PutEvents`, `BatchWriteItem` and `BatchGetItem` per entry; a bounded read says whether it is complete.

It is deliberately **not** a web framework: one Lambda serves one operation,
so there is no router, no middleware stack, no dependency-injection container
and no ORM. It adds structure at compile time and about a microsecond per
request at run time: a function that only answers HTTP ships as a 0.5–0.75 MB
zip and starts cold in well under 150 ms.

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

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["http"] }
```

## Features

Default features are empty; each capability links only what it names.

| Feature | Enables |
| --- | --- |
| `runtime` | the native Lambda loop over any typed payload |
| `http` | the buffered API Gateway / Function URL pipeline, access control, rate limiting, partial responses |
| `http-stream` | streamed JSON and server-sent events, with CORS and content negotiation |
| `apigw-rest` | REST API events next to HTTP API |
| `streaming` | response bodies that own their producer |
| `queue`, `queue-visibility` | SQS partial batches, visibility changes |
| `event`, `schedule` | typed EventBridge and scheduled payloads |
| `aws` | SDK configuration from the Lambda environment |
| `dynamo` | bounded DynamoDB reads and batches, page tokens, spans |
| `events` | EventBridge publishing with per-entry outcomes |
| `secrets` | typed Secrets Manager reads that never echo a value |
| `client` | outbound HTTP with byte and time limits |
| `auth` | RS256 / JWKS bearer-token verification |
| `cache`, `compression`, `digest` | in-process caches, bounded gzip, SHA-256 |
| `validate`, `problem` | Garde validation, RFC 9457 problem details |
| `logs`, `metrics`, `otel` | logs, CloudWatch EMF metrics, X-Ray traces |
| `mcp`, `mcp-openapi` | MCP servers for AI clients: hand-written tools, or tools from an OpenAPI document |
| `test-support` | synthetic invocations and requests for tests |

## Documentation

The [guide](https://eusoumaxi.github.io/davidrs/davidrs/guide/index.html) explains why the crate exists, how each pipeline works and how to use every capability; its examples are compiled and run as tests. The `examples/` directory has one runnable program per trigger.

## Development

```bash
scripts/check.sh            # everything CI runs
scripts/check.sh coverage   # line coverage
```

Rust 1.98.1 is pinned in `rust-toolchain.toml`. Read [CONTRIBUTING.md](CONTRIBUTING.md) before changing code, and [SECURITY.md](SECURITY.md) before reporting a vulnerability.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this crate by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
