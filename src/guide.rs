//! The developer guide: why `davidrs` exists, how it is built, and how to use
//! each capability.
//!
//! The rest of this site is the API reference. These chapters explain it, and
//! every code example in them is compiled and run as a test, so none of them
//! can drift from the code.
//!
//! # Read first
//!
//! 1. [`introduction`] — the problems the crate solves, and what it
//!    deliberately does not do.
//! 2. [`getting_started`] — a first function, choosing features, running it
//!    locally.
//! 3. [`deployment`] — Cargo Lambda, project layout, package sizes, and how
//!    a function connects to API Gateway, Function URLs, CloudFront, SQS and
//!    EventBridge.
//! 4. [`architecture`] — the invocation, the pipelines, failures and the
//!    extension points.
//! 5. [`aws_security`] — the recommended AWS architecture: what the platform
//!    enforces before the function runs, and what the function still does.
//! 6. [`features`] — every feature and exactly what it links.
//!
//! # One chapter per capability
//!
//! | Chapter | Feature | Covers |
//! | --- | --- | --- |
//! | [`invocations`] | always | `Deadline`, `Invocation`, `Context`, `RuntimeError`, configuration |
//! | [`triggers`] | `runtime`, `event`, `schedule` | direct invocations, EventBridge targets, schedules |
//! | [`http`] | `http`, `validate`, `problem` | the buffered HTTP pipeline, failures and renderers |
//! | [`access`] | `http`, `auth`, `dynamo` | who may call, tenants, permissions, rate limits |
//! | [`aws_security`] | `http`, `auth`, `dynamo` | what API Gateway, WAF, CloudFront and Cognito do first, and what is left to the function |
//! | [`streaming`] | `http-stream`, `streaming` | streamed JSON, server-sent events, owned producers |
//! | [`partial_responses`] | `http` | `fields=a,b.c,-d` masks, pushed down to DynamoDB and SQL |
//! | [`validation`] | `http` | collecting every invalid field in one `400` |
//! | [`queues`] | `queue`, `queue-visibility` | SQS partial batches and visibility |
//! | [`aws_config`] | `aws` | SDK configuration from the Lambda environment |
//! | [`dynamodb`] | `dynamo` | bounded reads, batches, page tokens, spans |
//! | [`eventbridge`] | `eventbridge` | publishing with per-entry outcomes |
//! | [`secrets`] | `secrets` | reading secrets without echoing them |
//! | [`outbound_http`] | `client` | calling other services with byte and time limits |
//! | [`tokens`] | `auth` | RS256 / JWKS bearer-token verification |
//! | [`utilities`] | `cache`, `compression`, `digest` | caches, bounded gzip, SHA-256 |
//! | [`telemetry`] | `logs`, `metrics`, `otel` | logs, EMF metrics, X-Ray traces |
//! | [`testing`] | `test-support` | testing your own handlers, and how this crate is verified |
//! | [`mcp`] | `mcp`, `mcp-openapi` | MCP servers: tools for AI clients, OAuth for them |

#[cfg(feature = "http")]
#[doc = include_str!("../docs/introduction.md")]
pub mod introduction {}

#[cfg(all(feature = "http", feature = "logs"))]
#[doc = include_str!("../docs/getting-started.md")]
pub mod getting_started {}

#[cfg(all(feature = "http", feature = "logs"))]
#[doc = include_str!("../docs/deployment.md")]
pub mod deployment {}

#[cfg(feature = "http")]
#[doc = include_str!("../docs/architecture.md")]
pub mod architecture {}

#[doc = include_str!("../docs/features.md")]
pub mod features {}

#[cfg(feature = "runtime")]
#[doc = include_str!("../docs/invocations.md")]
pub mod invocations {}

#[cfg(all(feature = "runtime", feature = "event", feature = "schedule"))]
#[doc = include_str!("../docs/triggers.md")]
pub mod triggers {}

#[cfg(all(feature = "http", feature = "validate", feature = "problem"))]
#[doc = include_str!("../docs/http.md")]
pub mod http {}

#[cfg(feature = "http")]
#[doc = include_str!("../docs/access.md")]
pub mod access {}

#[cfg(all(feature = "http", feature = "auth", feature = "dynamo"))]
#[doc = include_str!("../docs/aws-security.md")]
pub mod aws_security {}

#[cfg(feature = "http-stream")]
#[doc = include_str!("../docs/streaming.md")]
pub mod streaming {}

#[cfg(feature = "http")]
#[doc = include_str!("../docs/partial-responses.md")]
pub mod partial_responses {}

#[cfg(feature = "http")]
#[doc = include_str!("../docs/validation.md")]
pub mod validation {}

#[cfg(feature = "queue-visibility")]
#[doc = include_str!("../docs/queues.md")]
pub mod queues {}

#[cfg(feature = "aws")]
#[doc = include_str!("../docs/aws-config.md")]
pub mod aws_config {}

#[cfg(feature = "dynamo")]
#[doc = include_str!("../docs/dynamodb.md")]
pub mod dynamodb {}

#[cfg(feature = "eventbridge")]
#[doc = include_str!("../docs/eventbridge.md")]
pub mod eventbridge {}

#[cfg(feature = "secrets")]
#[doc = include_str!("../docs/secrets.md")]
pub mod secrets {}

#[cfg(feature = "client")]
#[doc = include_str!("../docs/outbound-http.md")]
pub mod outbound_http {}

#[cfg(all(feature = "auth", feature = "http"))]
#[doc = include_str!("../docs/tokens.md")]
pub mod tokens {}

#[cfg(all(feature = "cache", feature = "compression", feature = "digest"))]
#[doc = include_str!("../docs/utilities.md")]
pub mod utilities {}

#[cfg(all(feature = "logs", feature = "metrics", feature = "otel"))]
#[doc = include_str!("../docs/telemetry.md")]
pub mod telemetry {}

#[cfg(all(feature = "test-support", feature = "http"))]
#[doc = include_str!("../docs/testing.md")]
pub mod testing {}

#[cfg(all(
    feature = "mcp-openapi",
    feature = "auth",
    feature = "digest",
    feature = "logs"
))]
#[doc = include_str!("../docs/mcp.md")]
pub mod mcp {}
