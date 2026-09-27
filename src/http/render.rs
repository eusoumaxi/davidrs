//! Turning a [`Failure`] into a response, a step that cannot fail.
//!
//! A renderer that could fail would need a renderer of its own, so
//! [`ErrorRenderer::render`] returns a response unconditionally and the shared
//! [`literal`] builder has a format-neutral fallback.

use lambda_http::http::{header, StatusCode};
use lambda_http::{Body, Response};

use super::failure::Failure;
use super::response::{apply_headers, HttpResponse};

/// Renders failures into responses: the wire shape of every error an
/// endpoint returns.
///
/// Implement it to own a service's error contract; clients differ on error
/// envelopes more than on anything else.
///
/// # Examples
///
/// ```
/// use davidrs::http::{literal, ErrorRenderer, Failure, HttpResponse};
///
/// struct TextErrors;
///
/// impl ErrorRenderer for TextErrors {
///     fn render(&self, failure: &Failure) -> HttpResponse {
///         let body = format!("{}: {}", failure.code(), failure.public_message());
///         literal(failure.status(), "text/plain", body)
///     }
/// }
/// ```
pub trait ErrorRenderer: Send + Sync + 'static {
    /// Renders a failure. Must not fail and must not panic.
    ///
    /// Read the message through [`Failure::public_message`], so a 5xx stays
    /// sanitized, and copy [`Failure::headers`] onto the response.
    fn render(&self, failure: &Failure) -> HttpResponse;
}

/// Builds a response from a status, a media type and an already-serialized
/// body.
///
/// It cannot fail: when the media type is not a valid header value, the
/// result is an empty `500`. That fallback is format-neutral on purpose, since
/// every renderer shares this helper and one renderer's envelope must not be
/// served under another's media type.
pub fn literal(status: StatusCode, content_type: &'static str, body: String) -> HttpResponse {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::Text(body))
        .unwrap_or_else(|_| {
            let mut response = Response::new(Body::Empty);
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
}

/// The default renderer: `{"errorCode": "...", "errorMessage": "..."}` as
/// `application/json`, with the failure's headers.
///
/// A flat error body is what most clients already parse. The body is built
/// with `serde_json`, so quotes or newlines in a message cannot break it. Use
/// `ProblemErrors` (feature `problem`) for RFC 9457 instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlainErrors;

impl ErrorRenderer for PlainErrors {
    fn render(&self, failure: &Failure) -> HttpResponse {
        let body = serde_json::json!({
            "errorCode": failure.code(),
            "errorMessage": failure.public_message(),
        })
        .to_string();
        let mut response = literal(failure.status(), "application/json", body);
        apply_headers(&mut response, failure.headers());
        response
    }
}

#[cfg(feature = "problem")]
mod problem {
    use super::{apply_headers, literal, ErrorRenderer, Failure, HttpResponse};

    /// An RFC 9457 `application/problem+json` renderer, with the failure's
    /// headers.
    ///
    /// `title` comes from the status and `detail` is the public message. The
    /// code appears only in `type`: without a base, `type` is omitted, which
    /// RFC 9457 reads as `about:blank`.
    ///
    /// # Examples
    ///
    /// ```
    /// use davidrs::http::{ErrorRenderer, Failure, ProblemErrors, StatusCode};
    ///
    /// let renderer = ProblemErrors::default().with_type_base("https://example.com/errors/");
    /// let response = renderer.render(&Failure::new(StatusCode::NOT_FOUND, "ERROR_NOT_FOUND", "No such order"));
    /// assert_eq!(response.headers()["content-type"], "application/problem+json");
    /// ```
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct ProblemErrors {
        type_base: Option<String>,
    }

    impl ProblemErrors {
        /// Sets `type` to this base followed by the failure's code, such as
        /// `https://example.com/errors/ERROR_NOT_FOUND`.
        ///
        /// A base that does not form a valid URI with the code leaves `type`
        /// out.
        #[must_use]
        pub fn with_type_base(mut self, base: impl Into<String>) -> Self {
            self.type_base = Some(base.into());
            self
        }
    }

    impl ErrorRenderer for ProblemErrors {
        fn render(&self, failure: &Failure) -> HttpResponse {
            let mut details = problem_details::ProblemDetails::from_status_code(failure.status())
                .with_detail(failure.public_message());
            if let Some(base) = &self.type_base {
                if let Ok(uri) =
                    format!("{base}{}", failure.code()).parse::<lambda_http::http::Uri>()
                {
                    details = details.with_type(uri);
                }
            }
            let body = serde_json::to_string(&details)
                .unwrap_or_else(|_| r#"{"title":"Internal Server Error","status":500}"#.to_owned());
            let mut response = literal(failure.status(), "application/problem+json", body);
            apply_headers(&mut response, failure.headers());
            response
        }
    }
}

#[cfg(feature = "problem")]
pub use problem::ProblemErrors;
