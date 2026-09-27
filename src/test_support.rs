//! Builders for testing handlers, without a live AWS account.
//!
//! This feature adds **synthetic input only**. It contains no way to
//! construct an authorized scope, skip a policy or reach a real service: a
//! test-only trust flag that exists in a production build is a vulnerability,
//! not a convenience.

use std::time::Duration;

use crate::{Deadline, Invocation};

/// An invocation with a 30-second budget, for tests that do not exercise
/// deadlines.
///
/// ```
/// use davidrs::{test_support, Context};
///
/// let context = Context::new(test_support::invocation("r-1"), ());
/// assert_eq!(context.invocation().request_id, "r-1");
/// assert!(!context.deadline().is_expired());
/// ```
#[must_use]
pub fn invocation(request_id: &str) -> Invocation {
    Invocation::new(request_id, Deadline::after(Duration::from_secs(30)))
}

/// An invocation whose budget has already run out.
#[must_use]
pub fn expired_invocation(request_id: &str) -> Invocation {
    Invocation::new(
        request_id,
        Deadline::at(std::time::Instant::now() - Duration::from_secs(1)),
    )
}

/// An invocation with `budget` left.
#[must_use]
pub fn invocation_with_budget(request_id: &str, budget: Duration) -> Invocation {
    Invocation::new(request_id, Deadline::after(budget))
}

#[cfg(feature = "http")]
mod http_support {
    use lambda_http::{Body, Request, RequestExt};

    /// A `GET` request carrying a Lambda context, so the pipeline sees a
    /// request id and a deadline as it would in Lambda.
    ///
    /// # Panics
    ///
    /// Panics when `uri` is not a valid URI.
    #[must_use]
    pub fn get(uri: &str) -> Request {
        with_context(
            lambda_http::http::Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::Empty)
                .expect("valid request"),
        )
    }

    /// A `POST` request with a JSON body and a Lambda context.
    ///
    /// # Panics
    ///
    /// Panics when `uri` is not a valid URI.
    #[must_use]
    pub fn post_json(uri: &str, body: &str) -> Request {
        with_context(
            lambda_http::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::Text(body.to_owned()))
                .expect("valid request"),
        )
    }

    /// Attaches a Lambda context with the request id `test-request-id` and a
    /// 30-second budget.
    #[must_use]
    pub fn with_context(request: Request) -> Request {
        let mut context = lambda_runtime::Context::default();
        context.request_id = "test-request-id".to_owned();
        context.deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64)
            + 30_000;
        request.with_lambda_context(context)
    }
}

#[cfg(feature = "http")]
pub use http_support::{get, post_json, with_context};
