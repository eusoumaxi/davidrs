//! Borrowed access to the request, with every typed read bounded.
//!
//! [`Request`] borrows the native `lambda_http::Request` rather than copying
//! it, so a policy can inspect the exact bytes (for a signature) while a
//! decoder produces owned values. Nothing here awaits: the payload is already
//! in memory.

use std::borrow::Cow;
use std::collections::HashMap;

use lambda_http::http::{HeaderMap, Method, StatusCode};
use lambda_http::request::RequestContext;
use lambda_http::RequestExt;
use serde::de::DeserializeOwned;

use super::codes;
use super::failure::{Failure, FailureKind};

/// The default cap on a request body, in bytes (1 MiB).
///
/// Deliberately below Lambda's own 6 MB payload limit: an endpoint that needs
/// more raises it with [`Api::body_limit`](super::Api::body_limit).
pub const DEFAULT_BODY_LIMIT: usize = 1024 * 1024;

/// A borrowed view of one HTTP request, with a body limit.
///
/// # Examples
///
/// ```
/// use davidrs::http::Request;
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct Page {
///     page: u32,
/// }
///
/// let native = lambda_http::http::Request::builder()
///     .uri("https://example.com/orders?page=2")
///     .body(lambda_http::Body::Empty)
///     .unwrap();
/// let page: Page = Request::new(&native).query().unwrap();
/// assert_eq!(page.page, 2);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    inner: &'a lambda_http::Request,
    body_limit: usize,
}

impl<'a> Request<'a> {
    /// Wraps a native request with [`DEFAULT_BODY_LIMIT`].
    pub fn new(inner: &'a lambda_http::Request) -> Self {
        Self {
            inner,
            body_limit: DEFAULT_BODY_LIMIT,
        }
    }

    /// Sets the body limit every bounded read checks.
    #[must_use]
    pub fn with_body_limit(mut self, limit: usize) -> Self {
        self.body_limit = limit;
        self
    }

    /// The native request, for anything this view does not expose.
    pub fn native(&self) -> &'a lambda_http::Request {
        self.inner
    }

    /// The request method.
    pub fn method(&self) -> &'a Method {
        self.inner.method()
    }

    /// All request headers.
    pub fn headers(&self) -> &'a HeaderMap {
        self.inner.headers()
    }

    /// One header as text, if present and valid UTF-8.
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.inner
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    }

    /// The exact body bytes, for signatures and hashing; empty when there is
    /// no body.
    ///
    /// This read is not bounded: the pipeline has already checked the body
    /// limit before any decoder runs.
    pub fn raw_body(&self) -> &'a [u8] {
        self.inner.body().as_ref()
    }

    /// The raw query string, without the leading `?`.
    pub fn query_string(&self) -> &'a str {
        self.inner.uri().query().unwrap_or_default()
    }

    /// The query parameters the gateway parsed, as `(name, value)` pairs with
    /// repetitions kept.
    ///
    /// Use it for lists that arrive as `?tag=a&tag=b`.
    pub fn query_pairs(&self) -> Vec<(String, String)> {
        self.inner
            .query_string_parameters()
            .iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    /// The path parameters the gateway matched, by name.
    pub fn path_parameters(&self) -> HashMap<String, String> {
        self.inner
            .path_parameters()
            .iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    /// Deserializes the path parameters into `T`.
    ///
    /// # Errors
    ///
    /// Returns a `400` ([`codes::INVALID_PATH`]) when a parameter is missing
    /// or has the wrong shape.
    pub fn path<T: DeserializeOwned>(&self) -> Result<T, Failure> {
        let parameters = self.inner.path_parameters();
        let pairs: Vec<_> = parameters.iter().collect();
        let encoded = serde_urlencoded::to_string(pairs).unwrap_or_default();
        serde_urlencoded::from_str(&encoded)
            .map_err(|_| decode_failure(codes::INVALID_PATH, "path"))
    }

    /// Deserializes the query string into `T`.
    ///
    /// # Errors
    ///
    /// Returns a `400` ([`codes::INVALID_QUERY`]) when a parameter is missing
    /// or has the wrong shape.
    pub fn query<T: DeserializeOwned>(&self) -> Result<T, Failure> {
        serde_urlencoded::from_str(self.query_string())
            .map_err(|_| decode_failure(codes::INVALID_QUERY, "query"))
    }

    /// The caller's IP address as the gateway saw it, or `"unknown"`.
    ///
    /// Reads `sourceIp` from an HTTP API or, with the `apigw-rest`
    /// feature, a REST API request context. A request with no context,
    /// an empty address or another gateway flavour gives `"unknown"`.
    /// Forwarded headers are ignored on purpose: trusting them needs a proxy
    /// policy of the application's own, and without one any caller could
    /// choose its own rate-limit key.
    #[must_use]
    pub fn source_ip(&self) -> String {
        let ip = match self.inner.request_context_ref() {
            Some(RequestContext::ApiGatewayV2(gateway)) => gateway.http.source_ip.as_deref(),
            #[cfg(feature = "apigw-rest")]
            Some(RequestContext::ApiGatewayV1(gateway)) => gateway.identity.source_ip.as_deref(),
            _ => None,
        };
        ip.filter(|ip| !ip.is_empty())
            .unwrap_or("unknown")
            .to_owned()
    }

    /// Deserializes a JSON body, bounded by the body limit.
    ///
    /// # Errors
    ///
    /// Returns a `413` when the body exceeds the limit, a `415` when it
    /// declares a media type other than JSON, and a `400` when it is empty or
    /// malformed.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, Failure> {
        self.json_limited(self.body_limit)
    }

    /// Deserializes a JSON body under a tighter byte limit.
    ///
    /// The effective limit is the smaller of `limit` and the request's own,
    /// and it is checked before parsing, so an oversized payload never
    /// reaches `serde_json`.
    ///
    /// # Errors
    ///
    /// Returns a `413` when the body exceeds the limit, a `415` when it
    /// declares a media type other than JSON, and a `400` when it is empty or
    /// malformed.
    pub fn json_limited<T: DeserializeOwned>(&self, limit: usize) -> Result<T, Failure> {
        self.with_body_limit(limit.min(self.body_limit))
            .check_body_limit()?;
        self.check_json_media_type()?;
        let bytes = self.raw_body();
        if bytes.is_empty() {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                codes::MALFORMED_BODY,
                "The request body is required",
            )
            .with_kind(FailureKind::Decode));
        }
        serde_json::from_slice(bytes).map_err(|_| decode_failure(codes::MALFORMED_BODY, "body"))
    }

    /// Checks the body against the limit without reading it.
    ///
    /// The pipeline calls this before any decoder, so a decoder that reads
    /// [`Request::raw_body`] is bounded too.
    ///
    /// # Errors
    ///
    /// Returns a `413` ([`codes::BODY_TOO_LARGE`]) when the body is larger
    /// than the limit; the limit is in the failure's detail.
    pub fn check_body_limit(&self) -> Result<(), Failure> {
        if self.raw_body().len() > self.body_limit {
            return Err(Failure::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                codes::BODY_TOO_LARGE,
                "The request body is too large",
            )
            .with_detail(format!("Body exceeds the {} byte limit", self.body_limit))
            .with_kind(FailureKind::Decode));
        }
        Ok(())
    }

    /// The body as strict UTF-8 text, for an application's own JSON decoder.
    ///
    /// Size and media type are checked as in [`Request::json`]; the caller
    /// still validates the JSON syntax and the input contract.
    ///
    /// # Errors
    ///
    /// Returns a `413` when the body exceeds the limit, a `415` when it
    /// declares a media type other than JSON, and a `400` when it is not
    /// UTF-8.
    pub fn json_text(&self) -> Result<&'a str, Failure> {
        self.check_body_limit()?;
        self.check_json_media_type()?;
        std::str::from_utf8(self.raw_body())
            .map_err(|_| decode_failure(codes::MALFORMED_BODY, "body"))
    }

    /// Refuses a body that declares a media type other than
    /// `application/json`.
    ///
    /// A missing `Content-Type` is accepted: many clients send JSON without
    /// declaring it. A different declared representation is unambiguous and
    /// is never parsed as JSON.
    fn check_json_media_type(&self) -> Result<(), Failure> {
        if self
            .media_type()
            .is_some_and(|media| media != "application/json")
        {
            return Err(Failure::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                codes::UNSUPPORTED_MEDIA_TYPE,
                "Expected application/json",
            )
            .with_kind(FailureKind::Decode));
        }
        Ok(())
    }

    /// The `Content-Type`, lowercased and without parameters.
    pub fn media_type(&self) -> Option<Cow<'a, str>> {
        let value = self.header("content-type")?;
        let base = value.split(';').next().unwrap_or_default().trim();
        Some(if base.chars().any(char::is_uppercase) {
            Cow::Owned(base.to_lowercase())
        } else {
            Cow::Borrowed(base)
        })
    }
}

#[cfg(feature = "validate")]
impl Request<'_> {
    /// Deserializes a JSON body and validates it with Garde.
    ///
    /// Validates the decoded value only. For rules that need a context, decode
    /// with [`Request::json`] and call `validate_with` in the handler.
    ///
    /// # Errors
    ///
    /// Returns the failures of [`Request::json`], or a `400`
    /// ([`codes::INVALID_BODY`]) when a constraint fails.
    pub fn validated_json<T>(&self) -> Result<T, Failure>
    where
        T: DeserializeOwned + garde::Validate<Context = ()>,
    {
        let value: T = self.json()?;
        value.validate().map_err(|_| {
            Failure::new(
                StatusCode::BAD_REQUEST,
                codes::INVALID_BODY,
                "Request validation failed",
            )
            .with_kind(FailureKind::Decode)
        })?;
        Ok(value)
    }
}

/// A `400` for a part of the request that did not deserialize.
fn decode_failure(code: &'static str, what: &str) -> Failure {
    Failure::new(
        StatusCode::BAD_REQUEST,
        code,
        format!("Invalid request {what}"),
    )
    .with_kind(FailureKind::Decode)
}
