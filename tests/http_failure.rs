//! Failures, error catalogs and the renderers that put them on the wire.
//!
//! The rule under test: a 5xx never shows the message it was built with, and
//! nothing a renderer does can change that.
#![cfg(feature = "http")]

use std::time::Duration;

use davidrs::http::{
    codes, literal, Body, ErrorCatalog, ErrorDefinition, ErrorRenderer, Failure, FailureKind,
    HttpResponse, PlainErrors, StatusCode, INTERNAL_MESSAGE,
};
use davidrs::RuntimeError;

fn body(response: &HttpResponse) -> &str {
    match response.body() {
        Body::Text(text) => text,
        other => panic!("expected a text body, got {other:?}"),
    }
}

#[test]
fn a_4xx_shows_its_message() {
    let failure = Failure::new(
        StatusCode::CONFLICT,
        "ERROR_ORDER_CLOSED",
        "The order is closed",
    );
    assert_eq!(failure.status(), StatusCode::CONFLICT);
    assert_eq!(failure.code(), "ERROR_ORDER_CLOSED");
    assert_eq!(failure.public_message(), "The order is closed");
    assert_eq!(failure.to_string(), "409 The order is closed");
    assert!(!failure.is_server_error());
}

#[test]
fn every_5xx_hides_the_message_it_was_built_with() {
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::GATEWAY_TIMEOUT,
    ] {
        let failure = Failure::new(status, "FAULT_STORE", "table orders-table not found");
        assert!(failure.is_server_error());
        assert_eq!(failure.public_message(), INTERNAL_MESSAGE, "{status}");
        assert!(!failure.to_string().contains("orders-table"), "{status}");
        assert!(
            failure.internal_detail().contains("orders-table"),
            "{status}"
        );
    }
}

#[test]
fn internal_keeps_its_detail_for_diagnostics_only() {
    let failure = Failure::internal("FAULT_STORE", "throttled by orders-table");
    assert_eq!(failure.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failure.public_message(), INTERNAL_MESSAGE);
    assert_eq!(failure.internal_detail(), "throttled by orders-table");
}

#[test]
fn internal_detail_joins_the_message_and_the_detail() {
    let both = Failure::new(StatusCode::BAD_REQUEST, "E", "Bad input").with_detail("field x");
    let detail_only = Failure::new(StatusCode::BAD_REQUEST, "E", "").with_detail("field x");
    let message_only = Failure::new(StatusCode::BAD_REQUEST, "E", "Bad input");
    assert_eq!(both.internal_detail(), "Bad input: field x");
    assert_eq!(detail_only.internal_detail(), "field x");
    assert_eq!(message_only.internal_detail(), "Bad input");
}

#[test]
fn from_error_keeps_the_whole_source_chain() {
    let cause = std::io::Error::other("connection reset");
    let error = RuntimeError::other("reading orders", cause);
    let failure = Failure::from_error("FAULT_STORE", &error);
    assert_eq!(failure.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failure.public_message(), INTERNAL_MESSAGE);
    let detail = failure.internal_detail();
    assert!(detail.contains("reading orders"), "{detail}");
    assert!(detail.contains("connection reset"), "{detail}");
}

/// A code's prefix says nothing about the status: `ERROR_TIMEOUT` is a
/// server-side failure.
#[test]
fn a_runtime_error_becomes_a_500_coded_by_what_went_wrong() {
    let deadline = Failure::from(RuntimeError::DeadlineExceeded {
        elapsed: Duration::from_millis(100),
    });
    assert_eq!(deadline.code(), codes::TIMEOUT);
    assert_eq!(deadline.kind(), FailureKind::Deadline);
    assert!(deadline.is_server_error());

    let limit = Failure::from(RuntimeError::LimitExceeded {
        kind: "decoded bytes",
        limit: 1024,
    });
    assert_eq!(limit.code(), codes::LIMIT_EXCEEDED);
    assert_eq!(limit.kind(), FailureKind::Handler);
    assert!(limit.internal_detail().contains("decoded bytes"));

    let other = Failure::from(RuntimeError::Configuration("TABLE is not set".to_owned()));
    assert_eq!(other.code(), codes::UNHANDLED);
    assert_eq!(other.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(other.public_message(), INTERNAL_MESSAGE);
}

#[test]
fn a_failure_is_a_handler_failure_until_its_kind_is_set() {
    let failure = Failure::new(StatusCode::NOT_FOUND, "ERROR_NOT_FOUND", "No such order");
    assert_eq!(failure.kind(), FailureKind::Handler);
    assert_eq!(
        failure.with_kind(FailureKind::Policy).kind(),
        FailureKind::Policy
    );
}

#[test]
fn malformed_headers_are_dropped_instead_of_panicking() {
    let failure = Failure::new(StatusCode::TOO_MANY_REQUESTS, "ERROR_BUSY", "Slow down")
        .with_header("Retry-After", "30")
        .with_header("bad header", "x")
        .with_header("x-ok", "line\nbreak")
        .with_headers([("x-first", "1"), ("x-second", "2")]);
    let names: Vec<&str> = failure
        .headers()
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(names, ["retry-after", "x-first", "x-second"]);
}

#[test]
fn a_catalog_builds_known_failures_and_a_safe_500_for_unknown_codes() {
    const ERRORS: &[ErrorDefinition] = &[ErrorDefinition::new(
        "ERROR_ORDER_NOT_FOUND",
        StatusCode::NOT_FOUND,
        "The order does not exist",
    )];
    let definition = ERRORS.definition("ERROR_ORDER_NOT_FOUND").expect("known");
    assert_eq!(definition.status, StatusCode::NOT_FOUND);
    assert_eq!(definition.message, "The order does not exist");

    let known = ERRORS.failure("ERROR_ORDER_NOT_FOUND");
    assert_eq!(known.status(), StatusCode::NOT_FOUND);
    assert_eq!(known.code(), "ERROR_ORDER_NOT_FOUND");
    assert_eq!(known.public_message(), "The order does not exist");

    let built_at_runtime = ErrorDefinition::new("ERROR_GONE", StatusCode::GONE, "Gone").failure();
    assert_eq!(built_at_runtime.status(), StatusCode::GONE);

    let unknown = ERRORS.failure("ERROR_TYPO");
    assert_eq!(unknown.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(unknown.code(), codes::UNHANDLED);
    assert_eq!(unknown.public_message(), INTERNAL_MESSAGE);
    assert!(unknown.internal_detail().contains("ERROR_TYPO"));
}

#[test]
fn plain_errors_render_the_code_and_a_sanitized_message() {
    let failure = Failure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "FAULT_STORE",
        "ResourceNotFoundException: orders-table",
    );
    let response = PlainErrors.render(&failure);
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(
        body(&response),
        r#"{"errorCode":"FAULT_STORE","errorMessage":"InternalServerError"}"#
    );
}

#[test]
fn plain_errors_keep_quotes_and_newlines_inside_the_envelope() {
    let failure = Failure::new(StatusCode::BAD_REQUEST, "E", "he said \"no\"\nand left");
    let response = PlainErrors.render(&failure);
    let parsed: serde_json::Value = serde_json::from_str(body(&response)).expect("valid JSON");
    assert_eq!(parsed["errorMessage"], "he said \"no\"\nand left");
}

#[test]
fn plain_errors_carry_the_failure_headers() {
    let failure = Failure::new(StatusCode::TOO_MANY_REQUESTS, "ERROR_BUSY", "Slow down")
        .with_header("Retry-After", "30");
    let response = PlainErrors.render(&failure);
    assert_eq!(response.headers()["retry-after"], "30");
}

#[test]
fn literal_builds_a_response_from_a_serialized_body() {
    let response = literal(StatusCode::CONFLICT, "text/plain", "closed".to_owned());
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(body(&response), "closed");
}

#[test]
fn literal_falls_back_to_an_empty_500_when_the_media_type_is_invalid() {
    let response = literal(StatusCode::OK, "text/plain\n", "hidden".to_owned());
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.body(), &Body::Empty);
    assert!(response.headers().is_empty());
}

#[cfg(feature = "problem")]
mod problem {
    use davidrs::http::{Body, ErrorRenderer, Failure, ProblemErrors, StatusCode};

    fn problem(renderer: &ProblemErrors, failure: &Failure) -> serde_json::Value {
        let response = renderer.render(failure);
        assert_eq!(
            response.headers()["content-type"],
            "application/problem+json"
        );
        let Body::Text(text) = response.body() else {
            panic!("expected a text body")
        };
        serde_json::from_str(text).expect("valid JSON")
    }

    #[test]
    fn a_problem_has_the_status_title_and_public_message_but_no_type_by_default() {
        let failure = Failure::new(StatusCode::NOT_FOUND, "ERROR_NOT_FOUND", "No such order");
        let body = problem(&ProblemErrors::default(), &failure);
        assert_eq!(body["status"], 404);
        assert_eq!(body["title"], "Not Found");
        assert_eq!(body["detail"], "No such order");
        assert!(body.get("type").is_none());
    }

    #[test]
    fn a_type_base_turns_the_code_into_the_type_uri() {
        let failure = Failure::new(StatusCode::NOT_FOUND, "ERROR_NOT_FOUND", "No such order");
        let renderer = ProblemErrors::default().with_type_base("https://example.com/errors/");
        let body = problem(&renderer, &failure);
        assert_eq!(body["type"], "https://example.com/errors/ERROR_NOT_FOUND");
    }

    #[test]
    fn a_base_that_forms_no_valid_uri_leaves_the_type_out() {
        let failure = Failure::new(StatusCode::NOT_FOUND, "ERROR_NOT_FOUND", "No such order");
        let renderer = ProblemErrors::default().with_type_base("not a uri/");
        assert!(problem(&renderer, &failure).get("type").is_none());
    }

    #[test]
    fn a_problem_hides_a_5xx_message_and_keeps_the_headers() {
        let failure = Failure::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "FAULT_STORE",
            "orders-table throttled",
        )
        .with_header("Retry-After", "5");
        let response = ProblemErrors::default().render(&failure);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "5");
        let Body::Text(text) = response.body() else {
            panic!("expected a text body")
        };
        assert!(!text.contains("orders-table"));
        assert!(text.contains("InternalServerError"));
    }
}
