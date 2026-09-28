//! What a handler returns when it cannot produce a success, and the catalog
//! that names those failures.
//!
//! A [`Failure`] carries a public message and a separate, optional detail.
//! The detail is never rendered; it exists for deliberate, reviewed
//! diagnostics. A 5xx also replaces its public message with
//! [`INTERNAL_MESSAGE`], because a server-side message is so often an upstream
//! error that reached the constructor by accident. No constructor can leak:
//! `Failure::new(StatusCode::INTERNAL_SERVER_ERROR, code, sdk_error.to_string())`
//! renders the fixed string.

use std::fmt;

use lambda_http::http::{HeaderName, HeaderValue, StatusCode};

/// The fixed body message every 5xx renders, whatever produced it.
pub const INTERNAL_MESSAGE: &str = "InternalServerError";

/// Where a failure came from. Used for logging and metrics, never rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FailureKind {
    /// The request could not be admitted (rate limit, quota).
    Admission,
    /// The request could not be decoded or failed validation.
    Decode,
    /// Authentication or authorization refused the request.
    Policy,
    /// The handler refused the request for a domain reason.
    Handler,
    /// The success value could not be serialized.
    Serialization,
    /// The invocation budget ran out.
    Deadline,
}

/// A failure on its way to the client: status, stable code, public message,
/// internal detail, kind and headers.
///
/// Renderers read the message only through [`Failure::public_message`], which
/// is fixed for every 5xx.
///
/// # Examples
///
/// ```
/// use davidrs::http::{Failure, StatusCode, INTERNAL_MESSAGE};
///
/// let refused = Failure::new(StatusCode::CONFLICT, "ERROR_ORDER_CLOSED", "The order is closed");
/// assert_eq!(refused.public_message(), "The order is closed");
///
/// let broken = Failure::new(StatusCode::BAD_GATEWAY, "FAULT_UPSTREAM", "connect 10.0.0.7 refused");
/// assert_eq!(broken.public_message(), INTERNAL_MESSAGE);
/// assert!(broken.internal_detail().contains("10.0.0.7"));
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct Failure {
    status: StatusCode,
    code: &'static str,
    message: String,
    detail: Option<String>,
    kind: FailureKind,
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl Failure {
    /// A failure with an explicit status and a client-safe message.
    ///
    /// A 5xx status discards `message` at render time; put anything
    /// diagnostic in [`Failure::with_detail`] instead of the message.
    #[must_use]
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            detail: None,
            kind: FailureKind::Handler,
            headers: Vec::new(),
        }
    }

    /// A `500` whose detail is kept for diagnostics and never rendered.
    #[must_use]
    pub fn internal(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code,
            message: INTERNAL_MESSAGE.to_owned(),
            detail: Some(detail.into()),
            kind: FailureKind::Handler,
            headers: Vec::new(),
        }
    }

    /// A `500` built from an error, keeping its whole `source` chain as the
    /// detail.
    #[must_use]
    pub fn from_error(code: &'static str, error: &dyn std::error::Error) -> Self {
        Self::internal(code, crate::error::chain(error))
    }

    /// Attaches internal diagnostic text, never rendered.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Records where the failure came from.
    #[must_use]
    pub fn with_kind(mut self, kind: FailureKind) -> Self {
        self.kind = kind;
        self
    }

    /// Adds a response header that survives rendering.
    ///
    /// An invalid name or value is dropped rather than panicking: a malformed
    /// header must not turn a `429` into a crash.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            self.headers.push((name, value));
        }
        self
    }

    /// Adds several response headers, dropping invalid ones as
    /// [`Failure::with_header`] does.
    #[must_use]
    pub fn with_headers<I, K, V>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        for (name, value) in headers {
            self = self.with_header(name.as_ref(), value.as_ref());
        }
        self
    }

    /// The status that will be rendered.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// Where the failure came from.
    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    /// The headers a renderer puts on the response.
    pub fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }

    /// The message a client may see: the message for a 4xx, and
    /// [`INTERNAL_MESSAGE`] for every 5xx however the failure was built.
    ///
    /// Renderers must use this accessor for client-visible text.
    /// [`internal_detail`](Self::internal_detail) and `Debug` are diagnostic only.
    pub fn public_message(&self) -> &str {
        if self.status.is_server_error() {
            INTERNAL_MESSAGE
        } else {
            &self.message
        }
    }

    /// The unredacted message and the detail joined as `message: detail`, for
    /// a deliberate diagnostic. The message is left out when it is empty or
    /// [`INTERNAL_MESSAGE`].
    ///
    /// Never put this in a response.
    pub fn internal_detail(&self) -> String {
        match (&self.detail, self.message.as_str()) {
            (Some(detail), message) if message != INTERNAL_MESSAGE && !message.is_empty() => {
                format!("{message}: {detail}")
            }
            (Some(detail), _) => detail.clone(),
            (None, message) => message.to_owned(),
        }
    }

    /// Whether the status is a 5xx, the failures worth logging at error
    /// level.
    pub fn is_server_error(&self) -> bool {
        self.status.is_server_error()
    }
}

impl fmt::Display for Failure {
    /// Shows only what a client may see, so an accidental `{failure}` in a
    /// response body cannot disclose internals.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status.as_u16(), self.public_message())
    }
}

impl std::error::Error for Failure {}

/// A `500` whose detail is the error's source chain: `ERROR_TIMEOUT` with kind
/// [`FailureKind::Deadline`] for a deadline, `ERROR_LIMIT_EXCEEDED` for a
/// bounded limit, `FAULT_UNHANDLED` for anything else.
impl From<crate::RuntimeError> for Failure {
    fn from(error: crate::RuntimeError) -> Self {
        let code = match error {
            crate::RuntimeError::DeadlineExceeded { .. } => super::codes::TIMEOUT,
            crate::RuntimeError::LimitExceeded { .. } => super::codes::LIMIT_EXCEEDED,
            _ => super::codes::UNHANDLED,
        };
        let kind = match error {
            crate::RuntimeError::DeadlineExceeded { .. } => FailureKind::Deadline,
            _ => FailureKind::Handler,
        };
        Self::from_error(code, &error).with_kind(kind)
    }
}

/// One named failure in a service's catalog: code, status and client-safe
/// message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ErrorDefinition {
    /// The stable machine-readable code.
    pub code: &'static str,
    /// The HTTP status to render.
    pub status: StatusCode,
    /// The client-safe message.
    pub message: &'static str,
}

impl ErrorDefinition {
    /// Declares a failure; usable in a `const` table.
    pub const fn new(code: &'static str, status: StatusCode, message: &'static str) -> Self {
        Self {
            code,
            status,
            message,
        }
    }

    /// Builds a [`Failure`] from this definition.
    pub fn failure(&self) -> Failure {
        Failure::new(self.status, self.code, self.message)
    }
}

/// A set of named failures, so a service declares its wire errors in one
/// place.
///
/// The pipeline never requires a catalog. A `&[ErrorDefinition]` is one.
///
/// # Examples
///
/// ```
/// use davidrs::http::{ErrorCatalog, ErrorDefinition, StatusCode};
///
/// const ERRORS: &[ErrorDefinition] = &[ErrorDefinition::new(
///     "ERROR_ORDER_NOT_FOUND",
///     StatusCode::NOT_FOUND,
///     "The order does not exist",
/// )];
///
/// assert_eq!(ERRORS.failure("ERROR_ORDER_NOT_FOUND").status(), StatusCode::NOT_FOUND);
/// assert_eq!(ERRORS.failure("ERROR_TYPO").status(), StatusCode::INTERNAL_SERVER_ERROR);
/// ```
pub trait ErrorCatalog {
    /// Looks up a definition by code.
    fn definition(&self, code: &str) -> Option<ErrorDefinition>;

    /// Builds the failure a code names, or a `500` (`FAULT_UNHANDLED`) when
    /// the catalog does not know the code.
    fn failure(&self, code: &'static str) -> Failure {
        self.definition(code).map_or_else(
            || {
                Failure::internal(
                    super::codes::UNHANDLED,
                    format!("unknown error code {code}"),
                )
            },
            |definition| definition.failure(),
        )
    }
}

impl ErrorCatalog for &[ErrorDefinition] {
    fn definition(&self, code: &str) -> Option<ErrorDefinition> {
        self.iter().find(|entry| entry.code == code).copied()
    }
}
