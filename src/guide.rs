//! The developer guide: why `davidrs` exists, how an invocation moves through
//! it, and how to use each capability without rediscovering the failure it
//! was written to prevent.
//!
//! The rest of this site is the API reference. It describes every type. This
//! guide tells you which type to reach for, what to configure around it, what
//! a caller or Lambda should observe when it works, and what to change when
//! it does not. Every code example is compiled and run as a test, so a sample
//! that is printed here matches the crate.
//!
//! Each chapter stands on its own. The first paragraph says when to read it.
//! You do not have to read the guide in order after [getting started](getting_started).
//!
//! # Read this first
//!
//! Follow this path once, the first time you use the crate:
//!
//! 1. [`introduction`] — the failures a hand-written function usually ships
//!    with, what this crate takes over, and when you should use something else.
//! 2. [`getting_started`] — one HTTP function, from an empty crate to a local
//!    request, including the response body and the first errors you will hit.
//! 3. [`features`] — the `features` list in `Cargo.toml`, and why a workspace
//!    build can hide a missing one.
//! 4. The chapter for the trigger you are actually deploying (the table
//!    below).
//! 5. [`deployment`] — the binary, the front door in AWS, and the setting that
//!    makes the crate's partial-failure report do nothing if you forget it.
//! 6. [`testing`] — how to exercise the handler without an AWS account.
//!
//! Read [`architecture`] when you need the order of steps inside one
//! invocation. Read [`aws_security`] when you are deciding what CloudFront,
//! WAF, API Gateway and Cognito should refuse before the function runs.
//!
//! # Find a chapter by the job in front of you
//!
//! | You are trying to | Read |
//! | --- | --- |
//! | Answer API Gateway or a Function URL with one JSON response | [`http`] |
//! | Know who is calling, which tenant they act for, or how many times they may call | [`access`], then [`aws_security`] for what the platform should do first |
//! | Verify an RS256 bearer token inside the function | [`tokens`] |
//! | Stream JSON or server-sent events | [`streaming`] |
//! | Return only the JSON fields a client asked for | [`partial_responses`] |
//! | Report every invalid field in one `400` | [`validation`] |
//! | Consume an SQS queue without replaying the messages that succeeded | [`queues`] |
//! | Handle an EventBridge rule, a schedule, or a direct `Invoke` | [`triggers`] |
//! | Stop a retry from getting a fresh timeout, or fail startup when configuration is missing | [`invocations`] |
//! | Build AWS SDK clients from the Lambda environment | [`aws_config`] |
//! | Query DynamoDB with a limit, a page token, or an honest batch | [`dynamodb`] |
//! | Publish EventBridge events and see which entries were rejected | [`eventbridge`] |
//! | Load a secret once, without the value appearing in an error | [`secrets`] |
//! | Call another HTTP service with a size cap and a timeout | [`outbound_http`] |
//! | Cache a value, gzip a payload, or derive a SHA-256 key | [`utilities`] |
//! | Log, emit a CloudWatch metric, or join an X-Ray trace | [`telemetry`] |
//! | Let an AI client call your API as the signed-in user | [`mcp`] |
//!
//! # Every chapter
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
