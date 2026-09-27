# Features

Every capability is a Cargo feature, and default features are empty. A feature enables only what it names, plus the internal pieces it needs, so a function's `Cargo.toml` reads as its build: nothing it did not ask for is compiled in.

With no features at all, the crate is its value types — [`RuntimeError`](crate::RuntimeError), [`Invocation`](crate::Invocation), [`Deadline`](crate::Deadline), [`Context`](crate::Context) — and the environment readers. No runtime, no HTTP, no AWS SDK.

## What each feature enables and links

| Feature | Enables | Links, beyond the always-on `thiserror` |
| --- | --- | --- |
| `runtime` | `runtime::run`, invocation metadata, `Deadline::run` | `lambda_runtime`, `tokio` (rt, time), `serde`, `serde_json` |
| `http` | `http::Api`, `Request`, `Failure`, renderers, `access`, `RateLimited`, `fields`, `schema` | `runtime` + `lambda_http` (HTTP APIs, Function URLs), `serde_urlencoded`, `bytes`, `base64` |
| `http-stream` | `http::stream::StreamApi`, `Cors`, `negotiate`, server-sent events | `http` + `streaming` + `http-body`, `http-body-util` |
| `apigw-rest` | REST API events in both HTTP pipelines | `http` + `lambda_http/apigw_rest` |
| `streaming` | `streaming::StreamBody`, `Producer`, `streaming::run` | `runtime` + `futures-util`, `tokio-util` |
| `queue` | `queue::run`, `process`, `Delivery`, `Disposition` | `runtime` |
| `queue-visibility` | `queue::Visibility`, `MAX_VISIBILITY_TIMEOUT` | `queue` + `aws` + `aws-sdk-sqs` |
| `event` | `event::run`, `Event<T>` | `runtime` |
| `schedule` | `schedule::run` | `runtime` |
| `aws` | `aws::sdk_config`, `aws::Trust` | `aws-types`, `aws-credential-types`, `aws-smithy-http-client` (rustls with ring), `aws-smithy-async` (Tokio timer) |
| `dynamo` | `dynamo::*`, `http::rate_limit::DynamoWindow` with `http` | `aws` + `aws-sdk-dynamodb`, `serde_dynamo`, `base64`, `tracing` |
| `eventbridge` | `eventbridge::publish`, `publish_batch` | `aws` + `aws-sdk-eventbridge` |
| `secrets` | `secrets::string`, `secrets::json` | `aws` + `aws-sdk-secretsmanager` |
| `client` | `client::build`, `Limits`, `read_bounded`, `json_bounded`, `send_error` | `reqwest` (rustls, Mozilla roots), `rustls` (ring) |
| `auth` | `auth::Verifier`, `VerifiedClaims`, `bearer` | `client` + `ring`, `base64` |
| `cache` | `cache::Cache`, `CacheBuilder` | `moka` (future) |
| `compression` | `compression::gzip`, `gunzip_bounded` | `flate2` |
| `digest` | `digest::sha256`, `sha256_hex`, `hex` | `ring` |
| `validate` | `Request::validated_json` | `http` + `garde` (derive only) |
| `problem` | `http::ProblemErrors` (RFC 9457) | `http` + `problem_details` |
| `logs` | `telemetry::init`, `telemetry::logs`, invocation spans | `tracing`, `tracing-subscriber` (fmt) |
| `metrics` | `telemetry::Metrics` (CloudWatch EMF) | `serde`, `serde_json` |
| `otel` | X-Ray traces through the Lambda sandbox's UDP agent | `logs` + `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-aws`, `opentelemetry-proto`, `tracing-opentelemetry`, `prost` |
| `mcp` | `mcp::Server`, `mcp::Tool`, OAuth protected-resource metadata | `http` + `base64` |
| `mcp-openapi` | `mcp::openapi::OpenApi`: tools from an OpenAPI document, forwarded to the API | `mcp` + `client` |
| `test-support` | `test_support::*` | `serde_json` |

## Deliberate absences

- **No `regex`.** Several popular crates pull it in (about 240 KB in a release binary). `logs` reads a bare level from `RUST_LOG` instead of an `env-filter` expression, and `validate` enables only Garde's derive.
- **No `aws-config`.** The default credential chain's profile, SSO, IMDS and STS providers never run inside Lambda; `aws` reads the environment Lambda injects.
- **No certificate-store parsing at startup.** Both the SDK client (`aws`) and `client` use compiled-in or caller-supplied roots, which keeps tens of milliseconds off every cold start.

## Checking a function's features

In a workspace, Cargo unifies features across the members it builds, which can hide a missing one. Build each function alone — `cargo check -p <function>` — the way `cargo lambda build --package <function>` does. This crate's own CI builds and tests every feature alone for the same reason.
