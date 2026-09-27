//! A framework for AWS Lambda functions in Rust: fast, small and correct by
//! default, so the code you write is the business logic.
//!
//! `davidrs` is a thin layer over the official runtime crates: it keeps
//! [`lambda_runtime`]/[`lambda_http`] and the AWS SDK as the transport, adds
//! nothing at run time but a microsecond of pipeline per request, and owns
//! what every function otherwise rewrites — and usually gets subtly wrong —
//! around them:
//!
//! - **One absolute deadline per invocation.** [`Deadline`] is a monotonic
//!   instant; child budgets cannot outlive their parent, and every adapter
//!   keeps a cleanup margin before Lambda's own timeout.
//! - **One ordered pipeline per trigger.** HTTP requests ([`http::Api`]
//!   buffered, [`http::stream::StreamApi`] streamed) run admission, decoding,
//!   authorization and the handler in a fixed order, and every failure
//!   reaches one error renderer. SQS ([`queue`]), EventBridge ([`event`]),
//!   schedules ([`schedule`]) and direct invocations ([`runtime`]) decode
//!   their payload before the handler and report its failures to Lambda.
//! - **Failures that cannot leak.** A [`http::Failure`] carries a public
//!   message and a separate internal detail; any 5xx renders a fixed string no
//!   matter how it was built.
//! - **Bounded work everywhere.** Request and response bodies, decompressed
//!   payloads, paginated reads, retries and stream producers all have explicit
//!   limits and report when they hit them.
//! - **Honest partial outcomes.** SQS batches report failures per record,
//!   `PutEvents` and `BatchWriteItem` per entry, a bounded read says whether it
//!   is complete.
//! - **Access and limits by configuration.** [`http::access::Access`] decides
//!   who is calling, for which tenant and with what permission;
//!   [`http::RateLimited`] counts requests across every instance.
//!
//! It is deliberately not a web framework: one Lambda serves one operation, so
//! there is no router, no middleware stack, no dependency-injection container
//! and no ORM. Application state is an ordinary struct the handler receives as
//! `Arc<App>`; persistence uses the SDK's own builders.
//!
//! # Features
//!
//! `default = []`. The empty crate is [`RuntimeError`], [`Invocation`],
//! [`Deadline`], [`Context`] and the environment readers — value types with no
//! runtime, no HTTP and no AWS SDK. Every capability is additive, named after
//! what it enables, and never quietly enables an unrelated one: a binary
//! compiles exactly the capabilities its `Cargo.toml` asks for.
//!
//! | Feature | Enables |
//! | --- | --- |
//! | `runtime` | [`runtime::run`]: the native Lambda loop over a typed payload |
//! | `http` | [`http::Api`]: the buffered API Gateway / Function URL pipeline, [`http::access`], [`http::RateLimited`], [`http::fields`] |
//! | `http-stream` | [`http::stream`]: streamed JSON and server-sent events with CORS and content negotiation |
//! | `apigw-rest` | REST API proxy events next to HTTP API |
//! | `streaming` | [`streaming`]: response bodies that own their producer |
//! | `queue`, `queue-visibility` | [`queue`]: SQS partial-batch processing, visibility changes |
//! | `event`, `schedule` | [`event`], [`schedule`]: typed EventBridge and scheduled payloads |
//! | `aws` | [`aws::sdk_config`]: SDK configuration from the Lambda environment |
//! | `dynamo` | [`dynamo`]: bounded DynamoDB reads and writes, cursors, spans |
//! | `eventbridge` | [`eventbridge`]: EventBridge publishing with per-entry outcomes |
//! | `secrets` | [`secrets`]: typed Secrets Manager reads that never echo a value |
//! | `client` | [`client`]: an outbound `reqwest` client with byte and time limits |
//! | `auth` | [`auth`]: RS256 / JWKS token verification with a bounded refresh |
//! | `cache` | [`cache`]: in-process caches (Moka) |
//! | `compression` | [`compression`]: gzip with a cap on the decoded size |
//! | `digest` | [`digest`]: SHA-256 and hex for identifiers and cache keys |
//! | `validate`, `problem` | Garde validation and RFC 9457 problem details |
//! | `logs`, `metrics`, `otel` | [`telemetry`]: logs, EMF metrics and X-Ray traces |
//! | `test-support` | [`test_support`]: synthetic invocations and requests |
//! | `mcp`, `mcp-openapi` | [`mcp`]: an MCP server with hand-written tools, or tools from an OpenAPI document |
//!
//! # Where to start
//!
//! The [`guide`] explains why the crate exists, how the pipelines work and
//! how to use each capability, with examples that are compiled and tested.
//! `examples/` in the repository holds one runnable program per trigger.

mod context;
mod env;
mod error;

pub use context::{Context, Deadline, Invocation};
pub use env::{list_env, optional_env, required_env};
pub use error::{chain as error_chain, RuntimeError};

#[cfg(feature = "auth")]
pub mod auth;
#[cfg(feature = "aws")]
pub mod aws;
#[cfg(feature = "cache")]
pub mod cache;
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "compression")]
pub mod compression;
#[cfg(feature = "digest")]
pub mod digest;
#[cfg(feature = "dynamo")]
pub mod dynamo;
#[cfg(feature = "event")]
pub mod event;
#[cfg(feature = "eventbridge")]
pub mod eventbridge;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "queue")]
pub mod queue;
#[cfg(feature = "runtime")]
pub mod runtime;
#[cfg(feature = "schedule")]
pub mod schedule;
#[cfg(feature = "secrets")]
pub mod secrets;
#[cfg(feature = "streaming")]
pub mod streaming;
#[cfg(any(feature = "logs", feature = "metrics", feature = "otel"))]
pub mod telemetry;
#[cfg(feature = "test-support")]
pub mod test_support;

#[cfg(any(doc, doctest))]
pub mod guide;

/// The README's examples, compiled and run as doctests so they cannot drift.
#[cfg(all(doctest, feature = "http"))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
