//! [`RuntimeError`], the one error of startup and infrastructure paths.
//!
//! It carries no HTTP status and no SDK type, so the empty crate stays usable
//! as a plain dependency.

use std::fmt;
use std::time::Duration;

/// A failure of configuration, of a budget, of a bound, or of anything else
/// that is not a request's own fault.
///
/// Variants are additive: match with a `_` arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeError {
    /// Required configuration was missing or unusable.
    #[error("configuration: {0}")]
    Configuration(String),

    /// The invocation ran out of its absolute budget.
    #[error("deadline exceeded after {} ms", .elapsed.as_millis())]
    DeadlineExceeded {
        /// The time spent before the budget was exhausted.
        elapsed: Duration,
    },

    /// A bounded limit was hit (bytes, rows, pages, attempts).
    #[error("limit exceeded: {limit} of {kind}")]
    LimitExceeded {
        /// What was being counted, e.g. `"decoded bytes"`.
        kind: &'static str,
        /// The limit that was reached.
        limit: u64,
    },

    /// Anything the adapter could not classify. Keeps the source chain.
    #[error("{context}")]
    Other {
        /// Human-readable context supplied by the adapter.
        context: String,
        /// The underlying cause, when one exists.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

impl RuntimeError {
    /// Wraps any error with context, keeping it reachable through [`std::error::Error::source`].
    pub fn other<E>(context: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Other {
            context: context.into(),
            source: Some(Box::new(source)),
        }
    }

    /// A message-only failure, for cases with no underlying error value.
    pub fn message(context: impl Into<String>) -> Self {
        Self::Other {
            context: context.into(),
            source: None,
        }
    }
}

impl From<String> for RuntimeError {
    fn from(context: String) -> Self {
        Self::message(context)
    }
}

impl From<&str> for RuntimeError {
    fn from(context: &str) -> Self {
        Self::message(context)
    }
}

/// Formats an error and every `source` behind it as one line.
///
/// Use it to log a failure once with its full cause chain, instead of the
/// outermost message alone.
///
/// ```
/// # #[derive(Debug, thiserror::Error)] #[error("inner")] struct Inner;
/// # #[derive(Debug, thiserror::Error)] #[error("outer")] struct Outer(#[source] Inner);
/// assert_eq!(davidrs::error_chain(&Outer(Inner)), "outer: inner");
/// ```
pub fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        use fmt::Write as _;
        let _ = write!(text, ": {cause}");
        source = cause.source();
    }
    text
}
