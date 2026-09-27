//! The codes of the failures the pipelines raise themselves.
//!
//! Most codes come from the service, but a pipeline refuses some requests
//! before any handler runs: a body that is not JSON, one past the size limit,
//! a deadline that expired. Those failures use the codes below.
//!
//! A code is a stable identifier, not a retry instruction: `ERROR_TIMEOUT` is
//! a `504`. Clients decide what to retry from the status and the operation's
//! own contract, never from a prefix.
//!
//! # Using a different vocabulary
//!
//! An application whose clients expect other codes maps them in its
//! [`ErrorRenderer`](super::ErrorRenderer), which sees the whole
//! [`Failure`](super::Failure) and owns the wire:
//!
//! ```
//! use davidrs::http::{codes, literal, ErrorRenderer, Failure, HttpResponse};
//!
//! struct Snake;
//!
//! impl ErrorRenderer for Snake {
//!     fn render(&self, failure: &Failure) -> HttpResponse {
//!         let code = match failure.code() {
//!             codes::MALFORMED_BODY => "malformed_body",
//!             codes::BODY_TOO_LARGE => "body_too_large",
//!             other => other,
//!         };
//!         let body = serde_json::json!({"code": code}).to_string();
//!         literal(failure.status(), "application/json", body)
//!     }
//! }
//! ```

/// `400` — the path parameters did not deserialize into the target type.
pub const INVALID_PATH: &str = "ERROR_INVALID_PATH";

/// `400` — the query string did not deserialize into the target type.
pub const INVALID_QUERY: &str = "ERROR_INVALID_QUERY";

/// `400` — the body was absent, not UTF-8, or not valid JSON.
pub const MALFORMED_BODY: &str = "ERROR_MALFORMED_BODY";

/// `413` — the body is larger than the configured limit.
pub const BODY_TOO_LARGE: &str = "ERROR_BODY_TOO_LARGE";

/// `415` — the body declared a representation that is not JSON.
pub const UNSUPPORTED_MEDIA_TYPE: &str = "ERROR_UNSUPPORTED_MEDIA_TYPE";

/// `400` — the body parsed but failed validation.
pub const INVALID_BODY: &str = "ERROR_INVALID_BODY";

/// `404` — a handler returned `None`.
pub const NOT_FOUND: &str = "ERROR_NOT_FOUND";

/// `504` — the invocation ran out of its budget.
pub const TIMEOUT: &str = "ERROR_TIMEOUT";

/// `400` — the invocation payload is not an HTTP request the adapter reads.
pub const MALFORMED_REQUEST: &str = "ERROR_MALFORMED_REQUEST";

/// `403` — a browser origin outside the configured CORS allowlist.
pub const ORIGIN_NOT_ALLOWED: &str = "ERROR_ORIGIN_NOT_ALLOWED";

/// `405` — a method the endpoint does not serve.
pub const METHOD_NOT_ALLOWED: &str = "ERROR_METHOD_NOT_ALLOWED";

/// `400` — an `Accept` header with an unreadable quality value.
pub const INVALID_ACCEPT: &str = "ERROR_INVALID_ACCEPT";

/// `406` — an `Accept` header that excludes every representation served.
pub const NOT_ACCEPTABLE: &str = "ERROR_NOT_ACCEPTABLE";

/// `500` — a bounded limit was reached: bytes, rows, pages or attempts.
pub const LIMIT_EXCEEDED: &str = "ERROR_LIMIT_EXCEEDED";

/// `500` — the success value could not be serialized.
pub const SERIALIZATION: &str = "FAULT_SERIALIZATION";

/// `500` — anything this crate could not classify.
pub const UNHANDLED: &str = "FAULT_UNHANDLED";

/// `401` — a token was presented but does not verify or describes no caller
/// ([`access`](super::access)).
pub const INVALID_TOKEN: &str = "ERROR_INVALID_TOKEN";

/// `401` — the route requires a caller and none was identified
/// ([`access`](super::access)).
pub const UNAUTHENTICATED: &str = "ERROR_UNAUTHENTICATED";

/// `403` — the caller may not act for the tenant, or a permission rule refused
/// ([`access`](super::access)).
pub const FORBIDDEN: &str = "ERROR_FORBIDDEN";

/// `400` — the route requires a tenant and none was selected
/// ([`access`](super::access)).
pub const TENANT_REQUIRED: &str = "ERROR_TENANT_REQUIRED";

/// `429` — the caller passed a rate limit
/// ([`RateLimited`](super::RateLimited)).
pub const RATE_LIMITED: &str = "ERROR_RATE_LIMITED";
