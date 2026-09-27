//! Bounded, typed reads of one request through [`davidrs::http::Request`].
//!
//! Requests come either from a real API Gateway event parsed by `lambda_http`,
//! which is how a deployed function receives them, or from the `http` builder,
//! which is how a test or a direct call builds one.
#![cfg(feature = "http")]

use std::collections::HashMap;

use davidrs::http::{codes, Body, FailureKind, Method, Request, StatusCode};
use serde::Deserialize;

/// A request built by hand: no request context, no Lambda context.
fn built(content_type: Option<&str>, body: Body) -> lambda_http::Request {
    let mut builder = lambda_http::http::Request::builder()
        .method("POST")
        .uri("https://example.com/orders?page=3&q=widget");
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    builder.body(body).expect("request")
}

fn text(body: &str) -> lambda_http::Request {
    built(None, Body::Text(body.to_owned()))
}

/// An HTTP API event, parsed the way the runtime parses it.
fn http_api_event(source_ip: &str) -> lambda_http::Request {
    let event = serde_json::json!({
        "version": "2.0",
        "routeKey": "GET /orders/{id}",
        "rawPath": "/orders/42",
        "rawQueryString": "tag=a&tag=b",
        "headers": { "host": "example.com" },
        "pathParameters": { "id": "42" },
        "requestContext": {
            "accountId": "123456789012",
            "apiId": "api",
            "domainName": "example.com",
            "domainPrefix": "api",
            "http": {
                "method": "GET",
                "path": "/orders/42",
                "protocol": "HTTP/1.1",
                "sourceIp": source_ip,
                "userAgent": "test"
            },
            "requestId": "request-1",
            "routeKey": "GET /orders/{id}",
            "stage": "$default",
            "time": "01/Jan/2026:00:00:00 +0000",
            "timeEpoch": 1_767_225_600_000_u64
        },
        "isBase64Encoded": false
    });
    lambda_http::request::from_str(&event.to_string()).expect("HTTP API event")
}

#[derive(Debug, Deserialize)]
struct Page {
    page: u32,
    q: String,
}

#[derive(Debug, Deserialize)]
struct OrderPath {
    id: u32,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Item {
    name: String,
}

#[test]
fn the_view_exposes_the_method_headers_and_native_request() {
    let mut native = built(Some("application/json"), Body::Empty);
    native
        .headers_mut()
        .insert("x-binary", "caf\u{e9}".parse().expect("header"));
    native.headers_mut().insert(
        "x-opaque",
        lambda_http::http::HeaderValue::from_bytes(&[0xff]).expect("header"),
    );
    let request = Request::new(&native);
    assert_eq!(request.method(), Method::POST);
    assert!(std::ptr::eq(request.native(), &native));
    assert_eq!(request.headers().len(), 3);
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("x-missing"), None);
    assert_eq!(
        request.header("x-opaque"),
        None,
        "a header that is not UTF-8 reads as absent"
    );
}

#[test]
fn query_deserializes_the_query_string() {
    let native = built(None, Body::Empty);
    let request = Request::new(&native);
    assert_eq!(request.query_string(), "page=3&q=widget");
    let page: Page = request.query().expect("query");
    assert_eq!((page.page, page.q.as_str()), (3, "widget"));
}

#[test]
fn a_query_of_the_wrong_shape_is_a_400_decode_failure() {
    let native = lambda_http::http::Request::builder()
        .uri("https://example.com/orders?page=lots")
        .body(Body::Empty)
        .expect("request");
    let failure = Request::new(&native).query::<Page>().expect_err("400");
    assert_eq!(failure.status(), StatusCode::BAD_REQUEST);
    assert_eq!(failure.code(), codes::INVALID_QUERY);
    assert_eq!(failure.kind(), FailureKind::Decode);
    assert_eq!(failure.public_message(), "Invalid request query");
}

#[test]
fn query_pairs_keep_repeated_names() {
    let native = http_api_event("192.0.2.10");
    assert_eq!(
        Request::new(&native).query_pairs(),
        vec![
            ("tag".to_owned(), "a".to_owned()),
            ("tag".to_owned(), "b".to_owned())
        ]
    );
}

#[test]
fn path_parameters_deserialize_by_name() {
    let native = http_api_event("192.0.2.10");
    let request = Request::new(&native);
    assert_eq!(
        request.path_parameters(),
        HashMap::from([("id".to_owned(), "42".to_owned())])
    );
    assert_eq!(request.path::<OrderPath>().expect("path").id, 42);
}

#[test]
fn a_missing_path_parameter_is_a_400_decode_failure() {
    let native = built(None, Body::Empty);
    let failure = Request::new(&native).path::<OrderPath>().expect_err("400");
    assert_eq!(failure.status(), StatusCode::BAD_REQUEST);
    assert_eq!(failure.code(), codes::INVALID_PATH);
    assert_eq!(failure.kind(), FailureKind::Decode);
}

#[test]
fn source_ip_reads_an_http_api_request_context() {
    let native = http_api_event("192.0.2.10");
    assert_eq!(Request::new(&native).source_ip(), "192.0.2.10");
}

#[cfg(feature = "apigw-rest")]
#[test]
fn source_ip_reads_a_rest_api_request_context() {
    let event = serde_json::json!({
        "resource": "/orders",
        "path": "/orders",
        "httpMethod": "GET",
        "headers": { "X-Forwarded-For": "198.51.100.1" },
        "multiValueHeaders": { "X-Forwarded-For": ["198.51.100.1"] },
        "requestContext": {
            "accountId": "123456789012",
            "resourceId": "resource",
            "stage": "prod",
            "requestId": "request-1",
            "identity": { "sourceIp": "192.0.2.20" },
            "resourcePath": "/orders",
            "httpMethod": "GET",
            "apiId": "api"
        },
        "body": null,
        "isBase64Encoded": false
    });
    let native = lambda_http::request::from_str(&event.to_string()).expect("REST API event");
    assert_eq!(
        Request::new(&native).source_ip(),
        "192.0.2.20",
        "the gateway's address, not the forwarded header"
    );
}

/// A request built by hand carries no request context at all; reading its
/// address must not panic.
#[test]
fn source_ip_is_unknown_without_a_request_context() {
    let native = built(None, Body::Empty);
    assert_eq!(Request::new(&native).source_ip(), "unknown");
}

#[test]
fn source_ip_is_unknown_when_the_gateway_reports_an_empty_address() {
    let native = http_api_event("");
    assert_eq!(Request::new(&native).source_ip(), "unknown");
}

#[test]
fn json_deserializes_a_body_with_or_without_a_json_content_type() {
    for content_type in [None, Some("Application/JSON; charset=utf-8")] {
        let native = built(content_type, Body::Text(r#"{"name":"widget"}"#.to_owned()));
        let item: Item = Request::new(&native).json().expect("json");
        assert_eq!(item.name, "widget");
    }
}

#[test]
fn an_empty_json_body_is_a_400() {
    let native = built(None, Body::Empty);
    let failure = Request::new(&native).json::<Item>().expect_err("400");
    assert_eq!(failure.status(), StatusCode::BAD_REQUEST);
    assert_eq!(failure.code(), codes::MALFORMED_BODY);
    assert_eq!(failure.kind(), FailureKind::Decode);
    assert_eq!(failure.public_message(), "The request body is required");
}

#[test]
fn malformed_json_is_a_400_that_does_not_echo_the_body() {
    let native = text(r#"{"name": "submitted-value""#);
    let failure = Request::new(&native).json::<Item>().expect_err("400");
    assert_eq!(failure.code(), codes::MALFORMED_BODY);
    assert!(!failure.internal_detail().contains("submitted-value"));
}

#[test]
fn a_body_declared_as_another_media_type_is_a_415() {
    let native = built(Some("text/plain"), Body::Text("{}".to_owned()));
    let failure = Request::new(&native)
        .json::<serde_json::Value>()
        .expect_err("415");
    assert_eq!(failure.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(failure.code(), codes::UNSUPPORTED_MEDIA_TYPE);
}

/// The body is valid JSON, so only the size check can refuse it.
#[test]
fn a_body_over_the_limit_is_a_413_with_the_limit_in_the_detail() {
    let native = text(&format!("{{\"name\":\"{}\"}}", "x".repeat(200)));
    let failure = Request::new(&native)
        .json_limited::<Item>(64)
        .expect_err("413");
    assert_eq!(failure.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(failure.code(), codes::BODY_TOO_LARGE);
    assert!(failure.internal_detail().contains("64 byte limit"));
}

#[test]
fn a_body_exactly_at_the_limit_is_accepted() {
    let native = text(r#"{"a":1}"#);
    let value: serde_json::Value = Request::new(&native).json_limited(7).expect("at the limit");
    assert_eq!(value["a"], 1);
}

#[test]
fn json_limited_cannot_raise_the_request_limit() {
    let native = text(r#"{"a":1}"#);
    let failure = Request::new(&native)
        .with_body_limit(4)
        .json_limited::<serde_json::Value>(1024)
        .expect_err("413");
    assert_eq!(failure.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[test]
fn json_uses_the_limit_set_on_the_view() {
    let native = text(r#"{"a":1}"#);
    let failure = Request::new(&native)
        .with_body_limit(6)
        .json::<serde_json::Value>()
        .expect_err("413");
    assert_eq!(failure.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[test]
fn check_body_limit_refuses_one_byte_over() {
    let native = built(None, Body::Binary(vec![0; 5]));
    let request = Request::new(&native);
    assert!(request.with_body_limit(5).check_body_limit().is_ok());
    let failure = request
        .with_body_limit(4)
        .check_body_limit()
        .expect_err("413");
    assert_eq!(failure.kind(), FailureKind::Decode);
}

#[test]
fn json_text_returns_the_body_as_text() {
    let native = text(r#"{"name":"widget"}"#);
    assert_eq!(
        Request::new(&native).json_text().expect("text"),
        r#"{"name":"widget"}"#
    );
}

#[test]
fn json_text_refuses_bytes_that_are_not_utf8() {
    let native = built(None, Body::Binary(vec![0xff]));
    let failure = Request::new(&native).json_text().expect_err("400");
    assert_eq!(failure.status(), StatusCode::BAD_REQUEST);
    assert_eq!(failure.code(), codes::MALFORMED_BODY);
}

#[test]
fn json_text_checks_the_limit_and_the_media_type() {
    let native = text("{}");
    let failure = Request::new(&native)
        .with_body_limit(1)
        .json_text()
        .expect_err("413");
    assert_eq!(failure.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let native = built(Some("text/plain"), Body::Text("{}".to_owned()));
    let failure = Request::new(&native).json_text().expect_err("415");
    assert_eq!(failure.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[test]
fn raw_body_is_the_exact_bytes_of_every_body_kind() {
    let empty = built(None, Body::Empty);
    let spaced = text("{ \"a\" : 1 }");
    let binary = built(None, Body::Binary(vec![1, 2, 3]));
    assert_eq!(Request::new(&empty).raw_body(), b"");
    assert_eq!(Request::new(&spaced).raw_body(), b"{ \"a\" : 1 }");
    assert_eq!(Request::new(&binary).raw_body(), [1, 2, 3]);
}

#[test]
fn media_type_drops_parameters_and_case() {
    let absent = built(None, Body::Empty);
    let mixed = built(Some("Application/JSON; charset=utf-8"), Body::Empty);
    let lower = built(Some("text/csv"), Body::Empty);
    assert_eq!(Request::new(&absent).media_type(), None);
    assert_eq!(
        Request::new(&mixed).media_type().as_deref(),
        Some("application/json")
    );
    assert_eq!(
        Request::new(&lower).media_type().as_deref(),
        Some("text/csv")
    );
}

#[cfg(feature = "validate")]
mod validated {
    use davidrs::http::{codes, Body, FailureKind, Request, StatusCode};

    #[derive(Debug, serde::Deserialize, garde::Validate)]
    struct Line {
        #[garde(range(min = 1))]
        quantity: i32,
    }

    #[derive(Debug, serde::Deserialize, garde::Validate)]
    struct Order {
        #[garde(length(min = 1))]
        reference: String,
        #[garde(dive)]
        lines: Vec<Line>,
    }

    fn request(body: &str) -> lambda_http::Request {
        lambda_http::http::Request::builder()
            .method("POST")
            .uri("https://example.com/orders")
            .body(Body::Text(body.to_owned()))
            .expect("request")
    }

    #[test]
    fn a_valid_body_is_returned() {
        let native = request(r#"{"reference":"A-1","lines":[{"quantity":2}]}"#);
        let order: Order = Request::new(&native).validated_json().expect("valid");
        assert_eq!(order.reference, "A-1");
        assert_eq!(order.lines[0].quantity, 2);
    }

    #[test]
    fn a_nested_violation_is_a_400_invalid_body() {
        let native = request(r#"{"reference":"A-1","lines":[{"quantity":0}]}"#);
        let failure = Request::new(&native)
            .validated_json::<Order>()
            .expect_err("400");
        assert_eq!(failure.status(), StatusCode::BAD_REQUEST);
        assert_eq!(failure.code(), codes::INVALID_BODY);
        assert_eq!(failure.kind(), FailureKind::Decode);
        assert_eq!(failure.public_message(), "Request validation failed");
    }

    #[test]
    fn a_body_that_does_not_decode_is_refused_before_validation() {
        let native = request("not json");
        let failure = Request::new(&native)
            .validated_json::<Order>()
            .expect_err("400");
        assert_eq!(failure.code(), codes::MALFORMED_BODY);
    }
}
