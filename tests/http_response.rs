//! Success values becoming responses through [`davidrs::http::IntoResponse`].
//!
//! Serialization is fallible, and no wrapper may turn its failure into a
//! success.
#![cfg(feature = "http")]

use davidrs::http::{
    Body, FailureKind, HeaderMap, HeaderValue, HttpResponse, INTERNAL_MESSAGE, IntoResponse, Json,
    NoContent, StatusCode, codes,
};
use serde::{Serialize, Serializer};

/// A value that serializes to `{"id":1}`, or fails with text that must not
/// leak.
struct Payload {
    broken: bool,
}

impl Serialize for Payload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.broken {
            return Err(serde::ser::Error::custom("private upstream detail"));
        }
        serde_json::json!({"id": 1}).serialize(serializer)
    }
}

const GOOD: Payload = Payload { broken: false };
const BROKEN: Payload = Payload { broken: true };

#[test]
fn json_is_a_200_with_a_json_body() {
    let response = Json(GOOD).into_response().expect("ok");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.body(), &Body::Text(r#"{"id":1}"#.to_owned()));
}

#[test]
fn a_serialization_failure_is_a_sanitized_500() {
    let failure = Json(BROKEN).into_response().expect_err("must fail");
    assert_eq!(failure.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failure.code(), codes::SERIALIZATION);
    assert_eq!(failure.kind(), FailureKind::Serialization);
    assert_eq!(failure.public_message(), INTERNAL_MESSAGE);
    assert!(
        failure
            .internal_detail()
            .contains("private upstream detail")
    );
}

#[test]
fn a_chosen_status_cannot_hide_a_serialization_failure() {
    assert!((StatusCode::CREATED, Json(BROKEN)).into_response().is_err());
    assert!(
        (StatusCode::CREATED, HeaderMap::new(), Json(BROKEN))
            .into_response()
            .is_err()
    );
}

#[test]
fn a_status_tuple_replaces_the_status() {
    let response = (StatusCode::CREATED, Json(GOOD))
        .into_response()
        .expect("ok");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["content-type"], "application/json");
}

#[test]
fn a_status_and_headers_tuple_sets_headers_over_the_inner_ones() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/vnd.api+json"),
    );
    headers.insert("location", HeaderValue::from_static("/orders/7"));
    let response = (StatusCode::CREATED, headers, Json(GOOD))
        .into_response()
        .expect("ok");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers()["content-type"],
        "application/vnd.api+json"
    );
    assert_eq!(response.headers()["location"], "/orders/7");
}

#[test]
fn no_content_is_an_empty_204() {
    let response = NoContent.into_response().expect("ok");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.body(), &Body::Empty);
}

#[test]
fn a_built_response_passes_through_unchanged() {
    let mut built = HttpResponse::new(Body::Text("csv".to_owned()));
    *built.status_mut() = StatusCode::ACCEPTED;
    let response = built.into_response().expect("ok");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.body(), &Body::Text("csv".to_owned()));
}

#[test]
fn none_is_a_404_and_some_converts_its_value() {
    let failure = Option::<NoContent>::None.into_response().expect_err("404");
    assert_eq!(failure.status(), StatusCode::NOT_FOUND);
    assert_eq!(failure.code(), codes::NOT_FOUND);
    assert_eq!(failure.public_message(), "Not found");

    let response = Some(NoContent).into_response().expect("ok");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}
