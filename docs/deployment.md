# Deployment

From a crate that already runs on your machine to a function in AWS: Cargo Lambda, a workspace of one binary per operation, what those binaries weigh, and the setting on each front door that makes the crate's report visible to the platform. What WAF, Cognito and API Gateway should refuse before an invocation exists is [Security on AWS](crate::guide::aws_security).

## Tools

[Cargo Lambda](https://www.cargo-lambda.info) builds, runs and deploys Rust functions. It cross-compiles for Lambda's Linux with Zig, so a Mac or Windows machine builds the same `bootstrap` binary Lambda runs.

```bash
brew install cargo-lambda/tap/cargo-lambda   # installs Zig too; or: pip3 install cargo-lambda

cargo lambda new orders-get                        # a new function crate
cargo lambda watch                                 # every binary on a local emulator, :9000
cargo lambda invoke orders-get --data-file get.json   # one invocation with a payload you keep
cargo lambda build --release --arm64               # target/lambda/<function>/bootstrap
cargo lambda deploy orders-get                     # quick manual deploy of one function
```

Build for `arm64`: Graviton functions cost less per millisecond than x86 ones, and Rust gives up nothing on them. The runtime is `provided.al2023` with the handler `bootstrap`.

## Organizing many functions

One function serves one operation, so a real service has many small binaries. A Cargo workspace keeps them together:

```text
Cargo.toml                 the workspace: members, shared dependency versions, [profile.release]
crates/app/                your application layer: the Access policy, error catalog,
                           renderer, finalizer, App struct — everything the functions share
services/orders/core/      the domain: use cases, repositories, stored shapes
functions/orders-get/      one binary: main.rs is one call
functions/orders-create/
functions/orders-events/   an SQS or EventBridge consumer
```

Each function's `main.rs` only wires its pipeline:

```rust,no_run
# mod orders {
#     use std::sync::Arc;
#     use davidrs::http::{Failure, Json, Request};
#     use davidrs::{Context, RuntimeError};
#     pub struct App;
#     impl App { pub fn from_env() -> Result<Self, RuntimeError> { Ok(Self) } }
#     #[derive(serde::Deserialize)] pub struct GetOrder { pub id: String }
#     pub fn decode(request: &Request) -> Result<GetOrder, Failure> { request.path() }
#     pub async fn get(_app: Arc<App>, input: GetOrder, _context: Context<()>) -> Result<Json<String>, Failure> { Ok(Json(input.id)) }
# }
use std::sync::Arc;

use davidrs::http::{Api, PlainErrors, Public};
use davidrs::RuntimeError;

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders-get")?;
    let app = Arc::new(orders::App::from_env()?);
    Api::new("orders-get", Public, PlainErrors)
        .run(app, orders::decode, orders::get)
        .await
}
```

Two rules keep this layout honest:

- **Features per function.** Each function's `Cargo.toml` enables exactly the `davidrs` features it uses. Cargo unifies features across the members it builds together, which can hide a missing one, so check each function alone — `cargo check -p orders-get` — the way `cargo lambda build --package orders-get` builds it for deployment.
- **One release profile, at the root.** In a workspace, Cargo reads `[profile.*]` only from the root manifest; a profile in a member is ignored.

## The release profile

```toml
[profile.release]
opt-level = "z"        # smallest code; the functions are I/O-bound
lto = true
codegen-units = 1
panic = "abort"
strip = "symbols"
```

Cargo Lambda adds `strip`, `lto`, `codegen-units` and `panic` itself when the profile leaves them out. With `panic = "abort"`, a panic ends the sandbox and the next invocation starts cold; `davidrs` does not panic on a request path, and your handlers should not either — return a [`Failure`](crate::http::Failure) instead.

## Measuring a function

Build each function with its production features and record the compiler,
Cargo Lambda version, architecture and release profile alongside its package
size. Include every SDK client and telemetry feature used in production.
Compare cold `Init Duration`, warm duration, memory use and error rates under
the same memory setting and workload. Upstream latency and initialization
can dominate the result.

There is no published benchmark suite in this repository, so package sizes
and latency are not guarantees. `Trust::NativeRoots` reads the system trust
store; `Trust::Pem` and `client::build` use supplied or compiled-in roots.

## How a function connects

The recommended front door for an API is the platform itself:

```text
client ──▶ CloudFront ──▶ AWS WAF ──▶ API Gateway + Cognito authorizer ──▶ Lambda (davidrs)
           TLS, Shield    rate rules,   token checks, scopes,              what only the
           Standard,      managed       throttling, usage plans            application knows
           caching        rule groups
```

Everything to the left of the function refuses bad traffic before it costs an invocation; the function keeps the checks only the application can make. The [security chapter](crate::guide::aws_security) explains the split concern by concern. A web ACL attaches to a REST API stage and to a CloudFront distribution, never to an HTTP API or a Function URL: for those two, "WAF" means CloudFront in front.

### API Gateway HTTP API

```text
client ── HTTPS ──▶ API Gateway HTTP API ──▶ Lambda (payload 2.0) ──▶ Api
                     └─ JWT authorizer (Cognito or any OIDC issuer)
```

The usual front door for a buffered route. A JWT authorizer verifies the token before the function runs and passes its claims in the request context, where [`Access`](crate::http::access::Access) reads them — the function does no token work at all. Keep responses well inside the HTTP API's integration timeout (30 seconds, not raisable) and Lambda's 6 MB response limit. For a web ACL, put CloudFront in front.

### API Gateway REST API

```text
client ──▶ API Gateway REST API ──▶ Lambda (payload 1.0) ──▶ Api      (feature apigw-rest)
            └─ Cognito or Lambda authorizer   └─ STREAM integration ──▶ StreamApi
```

Choose it for what HTTP APIs lack: a web ACL on the stage itself, usage plans and API keys, request validation, private APIs, per-method caching, and response streaming through the `STREAM` response transfer mode (up to 15 minutes). Its integration timeout is 29 seconds by default, raisable for Regional and private APIs. Enable `apigw-rest` so both pipelines read version 1.0 payloads; a Cognito authorizer's claims arrive under `authorizer.claims`, which `Access` reads too.

### Function URLs

```text
client ──▶ https://<id>.lambda-url.<region>.on.aws ──▶ Lambda ──▶ Api or StreamApi
```

A URL on the function itself, with no gateway in between: nothing added to latency or cost. Buffered (`BUFFERED`) or streamed (`RESPONSE_STREAM`, served by [`StreamApi`](crate::http::stream::StreamApi)), with auth `NONE` or `AWS_IAM`. With `NONE`, the function is the only gate: verify bearer tokens with [`Access::verify_bearer`](crate::http::access::Access), grant the invoke permission only with the `lambda:InvokedViaFunctionUrl` condition, and cap the function with reserved concurrency — the only brake a Function URL has; past it, callers get `429`. For a web ACL, put CloudFront in front.

### CloudFront in front

```text
browser ──▶ CloudFront (custom domain, cache, WAF) ──▶ API Gateway (regional)
                                                   └─▶ Function URL (origin access control)
```

CloudFront gives one domain for the SPA and the API, edge caching and a WAF. Two details decide whether it works:

- **Origin access control signs the origin request in the `Authorization` header.** A viewer's bearer token must therefore travel in another header: a CloudFront Function on viewer request copies it, and [`StreamApi::prepare`](crate::http::stream::StreamApi::prepare) (or a decoder) restores it before the policy reads it. A `POST` or `PUT` through OAC also needs the viewer to send `x-amz-content-sha256`, so list it in [`Cors::allow_headers`](crate::http::stream::Cors::allow_headers).
- **The caller's address.** Behind CloudFront, the request context's source address is CloudFront's. Rate-limit on a header the edge writes and the viewer cannot set (CloudFront's `CloudFront-Viewer-Address`, or one a viewer function overwrites) with [`RateLimited::key`](crate::http::RateLimited::key).

### SQS

```text
producer ──▶ SQS queue ──▶ event source mapping ──▶ Lambda ──▶ queue::run
                            └─ FunctionResponseTypes: ReportBatchItemFailures
```

Turn on `ReportBatchItemFailures` in the event source mapping. Without it, Lambda ignores the per-record failures [`queue::run`](crate::queue::run) reports, treats the invocation as a success and deletes the whole batch, failed messages included. Give the queue a dead-letter queue and a `maxReceiveCount`, and a visibility timeout of at least six times the function timeout.

### EventBridge and schedules

```text
rule or EventBridge Scheduler ──▶ Lambda (async invoke) ──▶ event::run / schedule::run
```

Asynchronous invocations retry on failure (twice by default) and can send what still fails to a dead-letter queue or an on-failure destination: configure one, and make handlers idempotent.

## Permissions, tracing and configuration

- **One role per function**, granting exactly the actions and resources that operation uses: a read endpoint gets `dynamodb:GetItem` on one table, not `dynamodb:*`.
- **X-Ray.** The `otel` feature sends spans to the X-Ray agent inside the sandbox. Set the function's tracing mode to `Active`: in `PassThrough`, a function behind an HTTP API, a Function URL or SQS is never sampled.
- **Configuration** is environment variables read once in `main` with [`required_env`](crate::required_env), so a missing one fails the cold start instead of a request. Secrets come from Secrets Manager through [`secrets`](crate::secrets), never from plain variables.
- **Memory and timeout.** CPU scales with memory; 256–512 MB suits most functions. Set the function timeout from what the operation needs — every pipeline stops its own work before it, keeping 100 ms (one second when streaming) to answer.

## Infrastructure as code

With the AWS CDK, [`cargo-lambda-cdk`](https://github.com/cargo-lambda/cargo-lambda-cdk) builds each binary during synthesis:

```typescript
import { RustFunction } from "cargo-lambda-cdk";
import { Architecture, Tracing } from "aws-cdk-lib/aws-lambda";

new RustFunction(this, "OrdersGet", {
  manifestPath: "functions/orders-get/Cargo.toml",
  binaryName: "orders-get",
  architecture: Architecture.ARM_64,
  memorySize: 256,
  tracing: Tracing.ACTIVE,
  environment: { TABLE: table.tableName },
  bundling: { cargoLambdaFlags: ["--locked"] },
});
```

With AWS SAM, set `BuildMethod: rust-cargolambda` in the function's `Metadata`. With Terraform or any other tool, deploy the zip that `cargo lambda build --release --arm64 --output-format zip` writes.

## If the deploy looks healthy and the behaviour is wrong

| What you see | What it usually means | What to change |
| --- | --- | --- |
| The workspace build succeeds and `cargo lambda build --package <function>` fails | Another member enabled a feature this function forgot | `cargo check -p <function>` before you deploy |
| SQS deletes messages the handler never finished, or replays messages that succeeded | `ReportBatchItemFailures` is off, so Lambda ignores the partial-batch response | Turn it on in the event source mapping. Confirm the queue has a redrive policy. |
| Traces never appear for an HTTP API, a Function URL or an SQS consumer | Tracing mode is `PassThrough`, so the invocation is not sampled | Set tracing to `Active` on the function |
| A cold start fails immediately, naming a variable | `required_env` or `sdk_config` ran in `main` | Set the variable on the function. That failure is earlier than a request error, which is what you want. |
| The client sees a gateway `502` and your error renderer did not run | The function was killed at its timeout, or the handler returned an invocation error | Keep the function timeout below the gateway integration timeout. Return [`Failure`](crate::http::Failure) from an HTTP handler. |
| A panic takes the whole sandbox down and the next call is a cold start | `panic = "abort"` in the release profile | Return a [`Failure`](crate::http::Failure) or a [`RuntimeError`](crate::RuntimeError). Do not panic on a request path. |
| A Function URL has no authorizer, no WAF and no throttle | That is what a Function URL is | Put CloudFront and a web ACL in front, cap it with reserved concurrency, and verify tokens in the function. The [security chapter](crate::guide::aws_security) is the checklist. |
