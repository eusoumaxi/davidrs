//! Outbound HTTP with explicit limits.
//!
//! The returned value is `reqwest`'s own [`Client`](reqwest::Client): there is
//! no wrapper to learn and no middleware stack. This module adds the
//! configuration that is easy to forget and expensive to omit: a connect
//! timeout, a whole-request timeout, no automatic redirects, a body reader
//! that stops at a byte cap, and errors that never print the request URL.

use std::time::Duration;

use crate::RuntimeError;

/// Connection and request time limits for one client.
///
/// Body caps are not here: they are an argument of each bounded read, because
/// they depend on the response, not on the client. Build it with
/// [`Limits::new`] or [`Limits::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Cap on establishing a connection.
    pub connect: Duration,
    /// Cap on a whole request, including reading the body.
    pub request: Duration,
}

impl Limits {
    /// A cap on connecting and a cap on the whole request.
    #[must_use]
    pub const fn new(connect: Duration, request: Duration) -> Self {
        Self { connect, request }
    }
}

impl Default for Limits {
    /// 2 s to connect and 10 s for the whole request.
    ///
    /// A function with a 30 s budget should not spend all of it on one hung
    /// upstream.
    fn default() -> Self {
        Self::new(Duration::from_secs(2), Duration::from_secs(10))
    }
}

/// Builds a client over rustls, with `ring` and the Mozilla roots compiled in.
///
/// The TLS configuration is built here explicitly, so nothing is read from
/// the operating system's certificate store at startup, the binary runs on
/// a distroless image, and no
/// process-wide crypto provider is installed behind the caller's back.
/// Redirects are disabled: a redirect the application did not ask for
/// forwards custom credential headers such as `x-api-key` to whatever host the
/// upstream names, and turns one request into several the budget did not plan
/// for.
///
/// # Errors
///
/// Returns [`RuntimeError`] when the TLS configuration or the client cannot be
/// built.
///
/// # Examples
///
/// ```
/// use davidrs::client::{self, Limits};
///
/// let http = client::build(Limits::default())?;
/// # let _ = http;
/// # Ok::<(), davidrs::RuntimeError>(())
/// ```
pub fn build(limits: Limits) -> Result<reqwest::Client, RuntimeError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| RuntimeError::other("configuring TLS", error))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .connect_timeout(limits.connect)
        .timeout(limits.request)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| RuntimeError::other("building the HTTP client", error))
}

/// A `send()` failure as a [`RuntimeError`], without the request URL.
///
/// `reqwest::Error` prints its URL, and some APIs take a key as a query
/// parameter. Map every transport error through this before it can reach a
/// log.
#[must_use]
pub fn send_error(context: &str, error: reqwest::Error) -> RuntimeError {
    RuntimeError::other(context.to_owned(), error.without_url())
}

/// Reads a response body, refusing to buffer more than `limit` bytes.
///
/// A declared `Content-Length` over the limit fails before anything is read.
/// The body is then read chunk by chunk and each chunk is checked before it is
/// kept, so a missing or lying header cannot exhaust memory. The status is not
/// checked: a `404` or `500` body is read like any other.
///
/// # Errors
///
/// Returns [`RuntimeError::LimitExceeded`] when the body exceeds `limit`, or
/// another [`RuntimeError`], without the URL, when the transfer fails.
pub async fn read_bounded(
    response: reqwest::Response,
    limit: usize,
) -> Result<bytes::Bytes, RuntimeError> {
    use futures_util::StreamExt as _;

    if let Some(declared) = response.content_length()
        && declared > limit as u64
    {
        return Err(RuntimeError::LimitExceeded {
            kind: "response bytes",
            limit: limit as u64,
        });
    }
    let mut collected = bytes::BytesMut::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| send_error("reading the response body", error))?;
        if collected.len().saturating_add(chunk.len()) > limit {
            return Err(RuntimeError::LimitExceeded {
                kind: "response bytes",
                limit: limit as u64,
            });
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected.freeze())
}

/// Reads a bounded body and deserializes it as JSON.
///
/// # Errors
///
/// As [`read_bounded`], plus a failure when the body is not valid JSON for
/// `T`.
pub async fn json_bounded<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    limit: usize,
) -> Result<T, RuntimeError> {
    let body = read_bounded(response, limit).await?;
    serde_json::from_slice(&body)
        .map_err(|error| RuntimeError::other("decoding the response body", error))
}
