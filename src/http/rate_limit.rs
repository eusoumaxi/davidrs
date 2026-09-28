//! Rate limiting: the admission stage that counts requests, the counters, and
//! the headers a client reads.
//!
//! [`RateLimited`] is an [`Admission`] for [`Api`](super::Api): it counts each
//! request under a key (the caller's address by default) with a [`Counter`],
//! refuses past the limit with a `429`, and reports the budget on every
//! response. The counter is pluggable; [`DynamoWindow`] (feature `dynamo`)
//! counts in a DynamoDB table so the limit holds across every concurrent
//! Lambda instance, which an in-memory counter cannot do.
//!
//! Whatever counts, the client sees the same two fields of the IETF
//! `RateLimit` header draft:
//!
//! ```text
//! RateLimit-Policy: "default";q=60;w=300
//! RateLimit: "default";r=7;t=90
//! ```
//!
//! `RateLimit-Policy` states the quota (`q` requests per `w` seconds) and
//! `RateLimit` what is left of it (`r`) and when the window resets (`t`
//! seconds). They belong on the refusal as well as on the success: the caller
//! that just got a `429` is the one that most needs to know when to retry.
//!
//! The refusal's code and message are configurable with
//! [`RateLimited::refusal`], so an application keeps the ones its clients
//! already handle.
//!
//! ```
//! use std::time::{Duration, SystemTime};
//!
//! use davidrs::http::{RateLimit, RateLimitConfig};
//!
//! let config = RateLimitConfig::new("search", 60, Duration::from_secs(300));
//! let outcome = RateLimit::new(&config, 7, SystemTime::now() + Duration::from_secs(90));
//! let headers = outcome.headers();
//! assert_eq!(headers[0], ("RateLimit-Policy".to_owned(), "\"default\";q=60;w=300".to_owned()));
//! assert!(headers[1].1.starts_with("\"default\";r=7;t="));
//! assert!(!outcome.is_exceeded());
//! ```

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::StatusCode;
use super::codes;
use super::failure::{ErrorDefinition, Failure, FailureKind};
use super::policy::Admission;
use super::request::Request;
use crate::{Invocation, RuntimeError};

/// The name both header fields give the one policy a route applies.
const POLICY: &str = "default";

/// A route's rate-limit policy: `max_requests` per `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RateLimitConfig {
    /// Separates one route's counters from another's in the store.
    pub prefix: &'static str,
    /// Requests allowed per window.
    pub max_requests: u32,
    /// How long one window lasts.
    pub window: Duration,
}

impl RateLimitConfig {
    /// A policy of `max_requests` per `window`, counted under `prefix`.
    /// Usable in a `const`.
    #[must_use]
    pub const fn new(prefix: &'static str, max_requests: u32, window: Duration) -> Self {
        Self {
            prefix,
            max_requests,
            window,
        }
    }
}

/// The outcome of one rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RateLimit {
    /// Requests allowed per window.
    pub limit: u32,
    /// How long one window lasts.
    pub window: Duration,
    /// Requests left in this window. Negative once the budget is gone: a
    /// counter can run past the limit.
    pub remaining: i64,
    /// When the window resets.
    pub resets_at: SystemTime,
}

impl RateLimit {
    /// The outcome of a check against `config`: `remaining` requests left
    /// until the window resets at `resets_at`.
    #[must_use]
    pub fn new(config: &RateLimitConfig, remaining: i64, resets_at: SystemTime) -> Self {
        Self {
            limit: config.max_requests,
            window: config.window,
            remaining,
            resets_at,
        }
    }

    /// Whether this check refuses the request.
    #[must_use]
    pub fn is_exceeded(&self) -> bool {
        self.remaining < 0
    }

    /// The time left until the window resets, or [`Duration::ZERO`] once it
    /// has.
    #[must_use]
    pub fn reset_after(&self) -> Duration {
        self.resets_at
            .duration_since(SystemTime::now())
            .unwrap_or_default()
    }

    /// The `RateLimit-Policy` and `RateLimit` headers, in that order.
    ///
    /// Times are whole seconds, rounded up. The count is floored at zero,
    /// because the draft allows no negative values: a counter may run past
    /// the limit, the header may not. A zero window is left out of the
    /// policy.
    #[must_use]
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut policy = format!("\"{POLICY}\";q={}", self.limit);
        let window = whole_seconds(self.window);
        if window > 0 {
            policy.push_str(&format!(";w={window}"));
        }
        let state = format!(
            "\"{POLICY}\";r={};t={}",
            self.remaining.max(0),
            whole_seconds(self.reset_after())
        );
        vec![
            ("RateLimit-Policy".to_owned(), policy),
            ("RateLimit".to_owned(), state),
        ]
    }
}

/// A duration in whole seconds, rounded up, so a client that waits that long
/// never comes back early.
fn whole_seconds(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0))
}

/// Where requests are counted.
///
/// Implement it for any store — a cache cluster, a database — or use
/// [`DynamoWindow`]. A counter in the function's own memory only counts the
/// requests one Lambda instance happened to receive, so it is not a limit.
///
/// # Examples
///
/// ```
/// use std::time::{Duration, SystemTime};
///
/// use davidrs::http::{Counter, RateLimit, RateLimitConfig};
/// use davidrs::RuntimeError;
///
/// /// A store that always has three requests left, in a window that resets in
/// /// a minute.
/// struct Fixed;
///
/// impl Counter for Fixed {
///     async fn hit(&self, _key: &str, config: &RateLimitConfig) -> Result<RateLimit, RuntimeError> {
///         Ok(RateLimit::new(config, 3, SystemTime::now() + Duration::from_secs(60)))
///     }
/// }
/// ```
pub trait Counter: Send + Sync + 'static {
    /// Counts one request for `key` under `config`, and returns the budget
    /// that is left: [`RateLimit::new`] with the requests left after this
    /// one, negative once past the limit, and when the window resets.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`] when the store cannot be reached; the request
    /// is then refused with a `500`, never admitted uncounted.
    fn hit(
        &self,
        key: &str,
        config: &RateLimitConfig,
    ) -> impl Future<Output = Result<RateLimit, RuntimeError>> + Send;
}

type KeyOf = Arc<dyn Fn(&Request<'_>) -> String + Send + Sync>;

/// An [`Admission`] that limits requests per key.
///
/// ```no_run
/// # #[cfg(feature = "dynamo")]
/// # async fn build(client: aws_sdk_dynamodb::Client) {
/// use std::time::Duration;
///
/// use davidrs::http::{Api, PlainErrors, Public, RateLimitConfig, RateLimited};
/// use davidrs::http::rate_limit::DynamoWindow;
///
/// let limit = RateLimitConfig::new("search", 60, Duration::from_secs(60));
/// let api = Api::new("search", Public, PlainErrors)
///     .admission(RateLimited::new(DynamoWindow::new(client, "rate-limits"), limit));
/// # }
/// ```
pub struct RateLimited<C> {
    counter: C,
    config: RateLimitConfig,
    key: KeyOf,
    refusal: ErrorDefinition,
}

impl<C: Counter> RateLimited<C> {
    /// Limits requests per caller address ([`Request::source_ip`]) with the
    /// default refusal: `429` [`codes::RATE_LIMITED`].
    #[must_use]
    pub fn new(counter: C, config: RateLimitConfig) -> Self {
        Self {
            counter,
            config,
            key: Arc::new(|request: &Request<'_>| request.source_ip()),
            refusal: ErrorDefinition::new(
                codes::RATE_LIMITED,
                StatusCode::TOO_MANY_REQUESTS,
                "Too many requests",
            ),
        }
    }

    /// Counts under another key: an API key's id or hash, a tenant, a user id.
    ///
    /// The key is part of the stored counter's identity; keep it short and
    /// never put a secret in it.
    #[must_use]
    pub fn key(mut self, key: impl Fn(&Request<'_>) -> String + Send + Sync + 'static) -> Self {
        self.key = Arc::new(key);
        self
    }

    /// Replaces the refusal's code, status and message.
    #[must_use]
    pub fn refusal(mut self, refusal: ErrorDefinition) -> Self {
        self.refusal = refusal;
        self
    }
}

impl<C: Counter> Admission for RateLimited<C> {
    /// Counts the request; refuses it with the budget headers and a
    /// `Retry-After` (whole seconds, rounded up) once the limit is exceeded.
    async fn check(
        &self,
        request: &Request<'_>,
        _invocation: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        let key = (self.key)(request);
        let outcome = self
            .counter
            .hit(&key, &self.config)
            .await
            .map_err(|error| Failure::from(error).with_kind(FailureKind::Admission))?;
        let headers = outcome.headers();
        if outcome.is_exceeded() {
            return Err(self
                .refusal
                .failure()
                .with_kind(FailureKind::Admission)
                .with_header(
                    "retry-after",
                    &whole_seconds(outcome.reset_after()).to_string(),
                )
                .with_headers(headers));
        }
        Ok(headers)
    }
}

impl<C> std::fmt::Debug for RateLimited<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimited")
            .field("config", &self.config)
            .field("refusal", &self.refusal)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "dynamo")]
pub use dynamo::DynamoWindow;

#[cfg(feature = "dynamo")]
mod dynamo {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use aws_sdk_dynamodb::Client;
    use aws_sdk_dynamodb::types::{AttributeValue, ReturnValue};
    use tracing::Instrument as _;

    use super::{Counter, RateLimit, RateLimitConfig};
    use crate::RuntimeError;

    /// A fixed-window counter in a DynamoDB table.
    ///
    /// Each key and window is one item, incremented atomically with
    /// `UpdateItem … ADD` and expiring with the table's TTL, so the limit holds
    /// across every concurrent Lambda instance and costs one write per request.
    ///
    /// A window starts at a multiple of its length since the Unix epoch,
    /// both in whole milliseconds; a length under 1 ms counts as 1 ms. The
    /// item's key is `RATE_LIMIT#<prefix>#<key>` / `WINDOW#<window start>` in
    /// the attributes named by [`DynamoWindow::attributes`] (`PK`, `SK` and
    /// `ttl` by default), and it expires when its window resets; enable TTL
    /// on the table with that attribute.
    #[derive(Debug, Clone)]
    pub struct DynamoWindow {
        client: Client,
        table: String,
        partition: String,
        sort: String,
        ttl: String,
    }

    impl DynamoWindow {
        /// Counts in `table`, keyed by `PK` / `SK`, expiring by `ttl`.
        #[must_use]
        pub fn new(client: Client, table: impl Into<String>) -> Self {
            Self {
                client,
                table: table.into(),
                partition: "PK".to_owned(),
                sort: "SK".to_owned(),
                ttl: "ttl".to_owned(),
            }
        }

        /// Uses other key and TTL attribute names, to share a table whose
        /// keys are named differently.
        #[must_use]
        pub fn attributes(
            mut self,
            partition: impl Into<String>,
            sort: impl Into<String>,
            ttl: impl Into<String>,
        ) -> Self {
            self.partition = partition.into();
            self.sort = sort.into();
            self.ttl = ttl.into();
            self
        }
    }

    impl Counter for DynamoWindow {
        async fn hit(
            &self,
            key: &str,
            config: &RateLimitConfig,
        ) -> Result<RateLimit, RuntimeError> {
            let window = u64::try_from(config.window.as_millis())
                .unwrap_or(u64::MAX)
                .max(1);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64);
            let start = now / window * window;
            let reset = start.saturating_add(window);
            let output = self
                .client
                .update_item()
                .table_name(&self.table)
                .key(
                    &self.partition,
                    AttributeValue::S(format!("RATE_LIMIT#{}#{key}", config.prefix)),
                )
                .key(&self.sort, AttributeValue::S(format!("WINDOW#{start}")))
                .update_expression("SET #ttl = :ttl ADD #count :one")
                .expression_attribute_names("#ttl", &self.ttl)
                .expression_attribute_names("#count", "requestCount")
                .expression_attribute_values(
                    ":ttl",
                    AttributeValue::N(reset.div_ceil(1000).to_string()),
                )
                .expression_attribute_values(":one", AttributeValue::N("1".to_owned()))
                .return_values(ReturnValue::UpdatedNew)
                .send()
                .instrument(crate::dynamo::span("UpdateItem", &self.table))
                .await
                .map_err(|error| RuntimeError::other("counting a request", error))?;
            let count = output
                .attributes()
                .and_then(|attributes| attributes.get("requestCount"))
                .and_then(|value| value.as_n().ok())
                .and_then(|value| value.parse::<i64>().ok())
                .ok_or_else(|| RuntimeError::message("the rate-limit counter was not returned"))?;
            Ok(RateLimit::new(
                config,
                i64::from(config.max_requests).saturating_sub(count),
                UNIX_EPOCH + Duration::from_millis(reset),
            ))
        }
    }
}
