//! Turning a handler's success value into a response, fallibly.
//!
//! Serialization can fail. Swallowing that failure answers `200` with an
//! empty or truncated body, which is worse than a `500`, so
//! [`IntoResponse::into_response`] returns a `Result` and the pipeline renders
//! its failure like any other.

use lambda_http::http::header::CONTENT_TYPE;
use lambda_http::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use lambda_http::{Body, Response};
use serde::Serialize;

use super::codes;
use super::failure::{Failure, FailureKind};

/// The buffered response every pipeline step produces.
pub type HttpResponse = Response<Body>;

/// A success value that can become a response.
///
/// Implemented for [`Json`], [`NoContent`], an already-built
/// [`HttpResponse`], `(StatusCode, T)`, `(StatusCode, HeaderMap, T)` and
/// `Option<T>`. Implementations must not panic and must report a
/// serialization failure instead of hiding it.
pub trait IntoResponse {
    /// Converts the value into a response.
    ///
    /// # Errors
    ///
    /// Returns a [`Failure`] when the value cannot be serialized. The failure
    /// is a 500 whose detail is kept out of the body.
    fn into_response(self) -> Result<HttpResponse, Failure>;
}

/// A `200` with a JSON body, serialized with `serde_json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Json<T>(pub T);

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Result<HttpResponse, Failure> {
        let body = serde_json::to_string(&self.0).map_err(|error| {
            Failure::from_error(codes::SERIALIZATION, &error).with_kind(FailureKind::Serialization)
        })?;
        let mut response = Response::new(Body::Text(body));
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(response)
    }
}

/// An empty `204` response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoContent;

impl IntoResponse for NoContent {
    fn into_response(self) -> Result<HttpResponse, Failure> {
        let mut response = Response::new(Body::Empty);
        *response.status_mut() = StatusCode::NO_CONTENT;
        Ok(response)
    }
}

/// An already-built response passes through unchanged.
impl IntoResponse for HttpResponse {
    fn into_response(self) -> Result<HttpResponse, Failure> {
        Ok(self)
    }
}

/// `(status, value)` replaces the status of the inner response.
///
/// The inner conversion runs first, so a chosen status cannot mask a broken
/// body.
impl<T: IntoResponse> IntoResponse for (StatusCode, T) {
    fn into_response(self) -> Result<HttpResponse, Failure> {
        let (status, inner) = self;
        let mut response = inner.into_response()?;
        *response.status_mut() = status;
        Ok(response)
    }
}

/// `(status, headers, value)` replaces the status and sets the headers,
/// replacing any the inner response already had under the same names.
impl<T: IntoResponse> IntoResponse for (StatusCode, HeaderMap, T) {
    fn into_response(self) -> Result<HttpResponse, Failure> {
        let (status, headers, inner) = self;
        let mut response = inner.into_response()?;
        *response.status_mut() = status;
        response.headers_mut().extend(headers);
        Ok(response)
    }
}

/// `None` is a `404` ([`codes::NOT_FOUND`]); `Some` converts its value.
impl<T: IntoResponse> IntoResponse for Option<T> {
    fn into_response(self) -> Result<HttpResponse, Failure> {
        match self {
            Some(value) => value.into_response(),
            None => Err(Failure::new(
                StatusCode::NOT_FOUND,
                codes::NOT_FOUND,
                "Not found",
            )),
        }
    }
}

/// Sets each header on the response, replacing any with the same name.
pub(crate) fn apply_headers(response: &mut HttpResponse, headers: &[(HeaderName, HeaderValue)]) {
    for (name, value) in headers {
        response.headers_mut().insert(name.clone(), value.clone());
    }
}
