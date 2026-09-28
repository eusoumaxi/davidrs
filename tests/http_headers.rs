//! Repeated response headers, notably `set-cookie`, must reach the wire with
//! every value intact.
//!
//! `Failure::with_header` is additive in storage and documented as a header
//! that "survives rendering", and `Admission::check` returns a `Vec` that may
//! legitimately repeat a name. This file pins the contract that the renderer
//! and the admission-header loop preserve all values for a repeated name,
//! instead of collapsing them to the last one written.
#![cfg(feature = "http")]

use std::sync::Arc;

use davidrs::http::{
    Admission, Api, ErrorRenderer, Failure, Json, PlainErrors, Public, Request, StatusCode,
};
use davidrs::{Context, Invocation};

/// Collects every value held under one header name, in wire order.
fn values<'a>(response: &'a davidrs::http::HttpResponse, name: &str) -> Vec<&'a str> {
    response
        .headers()
        .get_all(name)
        .iter()
        .map(|value| value.to_str().expect("header value is text"))
        .collect()
}

/// Two `set-cookie` values on a `Failure` both reach the wire through the
/// default renderer, in insertion order; the renderer's own `content-type`
/// stays single.
#[test]
fn plain_errors_keep_every_value_of_a_repeated_set_cookie() {
    let failure = Failure::new(StatusCode::OK, "E", "ok")
        .with_header("set-cookie", "session=abc; Path=/; HttpOnly")
        .with_header("set-cookie", "tenant=xyz; Path=/; HttpOnly");
    let response = PlainErrors.render(&failure);
    assert_eq!(
        values(&response, "set-cookie"),
        [
            "session=abc; Path=/; HttpOnly",
            "tenant=xyz; Path=/; HttpOnly"
        ],
    );
    assert_eq!(values(&response, "content-type"), ["application/json"]);
}

/// An `Admission::check` that returns two pairs under `set-cookie` keeps both
/// on a successful response.
#[derive(Clone, Default)]
struct TwoCookies;

impl Admission for TwoCookies {
    async fn check(
        &self,
        _request: &Request<'_>,
        _invocation: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        Ok(vec![
            (
                "set-cookie".to_owned(),
                "session=abc; Path=/; HttpOnly".to_owned(),
            ),
            (
                "set-cookie".to_owned(),
                "tenant=xyz; Path=/; HttpOnly".to_owned(),
            ),
        ])
    }
}

async fn ok_handler(_: Arc<()>, _: (), _: Context<()>) -> Result<Json<()>, Failure> {
    Ok(Json(()))
}

fn post() -> lambda_http::Request {
    lambda_http::http::Request::builder()
        .method("POST")
        .uri("https://example.com/test")
        .body(davidrs::http::Body::Empty)
        .expect("request")
}

#[tokio::test]
async fn admission_can_repeat_set_cookie_on_a_success() {
    let api = Api::new("test", Public, PlainErrors).admission(TwoCookies);
    let response = api
        .handle(Arc::new(()), post(), &|_: &Request<'_>| Ok(()), &ok_handler)
        .await;
    assert_eq!(
        values(&response, "set-cookie"),
        [
            "session=abc; Path=/; HttpOnly",
            "tenant=xyz; Path=/; HttpOnly"
        ],
    );
}

/// An admission that refuses with its own repeated `set-cookie` headers keeps
/// every value on the rendered refusal.
#[derive(Clone, Default)]
struct RefusedWithTwoCookies;

impl Admission for RefusedWithTwoCookies {
    async fn check(
        &self,
        _request: &Request<'_>,
        _invocation: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        Err(
            Failure::new(StatusCode::TOO_MANY_REQUESTS, "ERROR_BUSY", "Slow down")
                .with_header("set-cookie", "clear=1; Path=/; Max-Age=0")
                .with_header("set-cookie", "flush=2; Path=/; Max-Age=0"),
        )
    }
}

#[tokio::test]
async fn an_admission_refusal_keeps_every_repeated_set_cookie() {
    let api = Api::new("test", Public, PlainErrors).admission(RefusedWithTwoCookies);
    let response = api
        .handle(Arc::new(()), post(), &|_: &Request<'_>| Ok(()), &ok_handler)
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        values(&response, "set-cookie"),
        ["clear=1; Path=/; Max-Age=0", "flush=2; Path=/; Max-Age=0"],
    );
}

/// Distinct admission header names stay single: `append` must not widen a name
/// that appears once into two.
#[derive(Clone, Default)]
struct DistinctBudget;

impl Admission for DistinctBudget {
    async fn check(
        &self,
        _request: &Request<'_>,
        _invocation: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        Ok(vec![
            ("ratelimit".to_owned(), "\"default\";r=9;t=60".to_owned()),
            ("ratelimit-policy".to_owned(), "9;w=60".to_owned()),
        ])
    }
}

#[tokio::test]
async fn distinct_admission_names_do_not_duplicate() {
    let api = Api::new("test", Public, PlainErrors).admission(DistinctBudget);
    let response = api
        .handle(Arc::new(()), post(), &|_: &Request<'_>| Ok(()), &ok_handler)
        .await;
    assert_eq!(values(&response, "ratelimit"), ["\"default\";r=9;t=60"]);
    assert_eq!(values(&response, "ratelimit-policy"), ["9;w=60"]);
    assert_eq!(values(&response, "content-type"), ["application/json"]);
}

#[cfg(feature = "problem")]
mod problem {
    use davidrs::http::{ErrorRenderer, Failure, ProblemErrors, StatusCode};

    use super::values;

    /// The RFC 9457 renderer shares `apply_headers`, so repeated values survive
    /// under it as well.
    #[test]
    fn problem_errors_keep_every_value_of_a_repeated_set_cookie() {
        let failure = Failure::new(StatusCode::OK, "E", "ok")
            .with_header("set-cookie", "session=abc; Path=/; HttpOnly")
            .with_header("set-cookie", "tenant=xyz; Path=/; HttpOnly");
        let response = ProblemErrors::default().render(&failure);
        assert_eq!(
            values(&response, "set-cookie"),
            [
                "session=abc; Path=/; HttpOnly",
                "tenant=xyz; Path=/; HttpOnly"
            ],
        );
        assert_eq!(
            values(&response, "content-type"),
            ["application/problem+json"],
        );
    }
}
