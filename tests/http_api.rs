//! The buffered pipeline, driven one request at a time through
//! [`davidrs::http::Api::handle`].
//!
//! Admission, decode, policy, handler and serialization run in that order
//! under one deadline; every failure reaches the one renderer and every
//! response reaches the finalizer. The policy and renderer below are written
//! the way an application writes its own, so whatever compiles here is what a
//! downstream crate can do.
#![cfg(feature = "http")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use davidrs::http::{
    Admission, Api, Body, ErrorRenderer, Failure, HeaderValue, HttpResponse, Json, PlainErrors,
    Policy, Public, Request, StatusCode,
};
use davidrs::{Context, Invocation};
use lambda_http::RequestExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct Item {
    name: String,
}

#[derive(Debug, Serialize)]
struct Created {
    name: String,
}

/// Counts handler calls, so a test can prove the handler never ran.
#[derive(Default)]
struct App {
    calls: AtomicUsize,
}

impl App {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

async fn create(app: Arc<App>, item: Item, _: Context<()>) -> Result<Json<Created>, Failure> {
    app.calls.fetch_add(1, Ordering::SeqCst);
    Ok(Json(Created { name: item.name }))
}

fn decode_item(request: &Request<'_>) -> Result<Item, Failure> {
    request.json::<Item>()
}

fn post(body: &str) -> lambda_http::Request {
    lambda_http::http::Request::builder()
        .method("POST")
        .uri("https://example.com/items")
        .body(Body::Text(body.to_owned()))
        .expect("request")
}

/// Attaches a Lambda context whose deadline is `budget_ms` from now.
fn with_budget(request: lambda_http::Request, budget_ms: u64) -> lambda_http::Request {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let mut context = lambda_runtime::Context::default();
    context.request_id = "request-1".to_owned();
    context.deadline = now + budget_ms;
    request.with_lambda_context(context)
}

fn text(response: &HttpResponse) -> &str {
    match response.body() {
        Body::Text(text) => text,
        other => panic!("expected a text body, got {other:?}"),
    }
}

fn api() -> Api<Public, PlainErrors> {
    Api::new("create-item", Public, PlainErrors)
}

/// Admits every request and reports a budget, the way a rate limiter does.
struct Budget;

impl Admission for Budget {
    async fn check(
        &self,
        _: &Request<'_>,
        _: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        Ok(vec![
            ("RateLimit".to_owned(), "\"default\";r=9;t=60".to_owned()),
            ("bad header".to_owned(), "dropped".to_owned()),
        ])
    }
}

/// Refuses every request with a `429`.
struct Exhausted;

impl Admission for Exhausted {
    async fn check(
        &self,
        _: &Request<'_>,
        _: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        Err(
            Failure::new(StatusCode::TOO_MANY_REQUESTS, "ERROR_BUSY", "Slow down")
                .with_header("Retry-After", "60"),
        )
    }
}

/// Never answers, so only the deadline can end the request.
struct Stuck;

impl Admission for Stuck {
    async fn check(
        &self,
        _: &Request<'_>,
        _: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        std::future::pending().await
    }
}

impl Policy for Stuck {
    type Scope = ();

    async fn authorize(&self, _: &Request<'_>, _: &Invocation) -> Result<(), Failure> {
        std::future::pending().await
    }
}

/// What [`UserHeader`] establishes. Its field is private, so a handler can
/// only receive one the policy produced.
#[derive(Debug)]
struct Caller {
    user: String,
}

/// An application's own authentication: a header checked against an
/// allowlist.
struct UserHeader {
    allowed: Vec<String>,
}

impl Policy for UserHeader {
    type Scope = Caller;

    async fn authorize(&self, request: &Request<'_>, _: &Invocation) -> Result<Caller, Failure> {
        let user = request.header("x-user").unwrap_or_default();
        if user.is_empty() {
            return Err(Failure::new(
                StatusCode::UNAUTHORIZED,
                "ERROR_UNAUTHORIZED",
                "Sign in first",
            ));
        }
        if !self.allowed.iter().any(|allowed| allowed == user) {
            return Err(Failure::new(
                StatusCode::FORBIDDEN,
                "ERROR_FORBIDDEN",
                "That user may not do this",
            ));
        }
        Ok(Caller {
            user: user.to_owned(),
        })
    }
}

/// An application's own error shape, which is not the crate's default.
struct XmlErrors;

impl ErrorRenderer for XmlErrors {
    fn render(&self, failure: &Failure) -> HttpResponse {
        davidrs::http::literal(
            failure.status(),
            "application/xml",
            format!(
                "<error code=\"{}\">{}</error>",
                failure.code(),
                failure.public_message()
            ),
        )
    }
}

#[derive(Debug, Serialize)]
struct Owned {
    user: String,
    name: String,
}

async fn create_owned(
    _: Arc<App>,
    item: Item,
    context: Context<Caller>,
) -> Result<Json<Owned>, Failure> {
    Ok(Json(Owned {
        user: context.scope().user.clone(),
        name: item.name,
    }))
}

fn guarded() -> Api<UserHeader, XmlErrors> {
    Api::new(
        "create-owned-item",
        UserHeader {
            allowed: vec!["ada".to_owned()],
        },
        XmlErrors,
    )
}

fn post_as(user: Option<&str>, body: &str) -> lambda_http::Request {
    let mut request = post(body);
    if let Some(user) = user {
        request
            .headers_mut()
            .insert("x-user", HeaderValue::from_str(user).expect("header"));
    }
    request
}

#[tokio::test]
async fn a_successful_request_renders_the_handler_value() {
    let app = Arc::new(App::default());
    let response = api()
        .handle(
            Arc::clone(&app),
            post(r#"{"name":"widget"}"#),
            &decode_item,
            &create,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text(&response), r#"{"name":"widget"}"#);
    assert_eq!(app.calls(), 1);
}

#[tokio::test]
async fn a_decode_failure_is_rendered_without_echoing_the_body_and_the_handler_never_runs() {
    let app = Arc::new(App::default());
    let response = api()
        .handle(
            Arc::clone(&app),
            post("\"secret-submitted-value\""),
            &decode_item,
            &create,
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(text(&response).contains("ERROR_MALFORMED_BODY"));
    assert!(!text(&response).contains("secret-submitted-value"));
    assert_eq!(app.calls(), 0);
}

/// The decoder reads raw bytes and would accept anything; the endpoint's
/// limit still applies, because the pipeline checks it first.
#[tokio::test]
async fn a_body_over_the_endpoint_limit_is_refused_before_the_decoder_runs() {
    let app = Arc::new(App::default());
    let response = api()
        .body_limit(3)
        .handle(
            Arc::clone(&app),
            post("1234"),
            &|_: &Request<'_>| -> Result<Item, Failure> { panic!("the decoder must not run") },
            &create,
        )
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(app.calls(), 0);
}

/// The body is not JSON, so a `429` proves admission ran before decoding.
#[tokio::test]
async fn admission_runs_before_decoding_and_its_refusal_keeps_its_headers() {
    let response = api()
        .admission(Exhausted)
        .handle(
            Arc::new(App::default()),
            post("not json"),
            &decode_item,
            &create,
        )
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "60");
}

#[tokio::test]
async fn admission_headers_reach_a_success_and_invalid_ones_are_dropped() {
    let response = api()
        .admission(Budget)
        .handle(
            Arc::new(App::default()),
            post(r#"{"name":"widget"}"#),
            &decode_item,
            &create,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["ratelimit"], "\"default\";r=9;t=60");
    assert_eq!(response.headers().len(), 2, "content-type and ratelimit");
}

#[tokio::test]
async fn admission_headers_reach_a_later_failure() {
    let response = api()
        .admission(Budget)
        .handle(
            Arc::new(App::default()),
            post("not json"),
            &decode_item,
            &create,
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()["ratelimit"], "\"default\";r=9;t=60");
}

#[tokio::test]
async fn a_serialization_failure_becomes_a_rendered_500_not_a_200() {
    struct Broken;
    impl Serialize for Broken {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("private upstream detail"))
        }
    }
    async fn broken(_: Arc<App>, _: Item, _: Context<()>) -> Result<Json<Broken>, Failure> {
        Ok(Json(Broken))
    }
    let response = api()
        .handle(
            Arc::new(App::default()),
            post(r#"{"name":"widget"}"#),
            &decode_item,
            &broken,
        )
        .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        text(&response),
        r#"{"errorCode":"FAULT_SERIALIZATION","errorMessage":"InternalServerError"}"#
    );
}

#[tokio::test]
async fn a_slow_admission_is_cut_off_with_a_504() {
    let app = Arc::new(App::default());
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        api().admission(Stuck).handle(
            Arc::clone(&app),
            with_budget(post(r#"{"name":"widget"}"#), 150),
            &decode_item,
            &create,
        ),
    )
    .await
    .expect("admission must not hang");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(text(&response).contains("ERROR_TIMEOUT"));
    assert_eq!(app.calls(), 0);
}

#[tokio::test]
async fn a_slow_policy_is_cut_off_with_a_504_that_keeps_admission_headers() {
    let app = Arc::new(App::default());
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        Api::new("stuck-policy", Stuck, PlainErrors)
            .admission(Budget)
            .handle(
                Arc::clone(&app),
                with_budget(post(r#"{"name":"widget"}"#), 150),
                &decode_item,
                &create,
            ),
    )
    .await
    .expect("the policy must not hang");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(response.headers()["ratelimit"], "\"default\";r=9;t=60");
    assert_eq!(app.calls(), 0);
}

#[tokio::test]
async fn a_slow_handler_is_cut_off_with_a_504() {
    async fn stuck(_: Arc<App>, _: Item, _: Context<()>) -> Result<Json<bool>, Failure> {
        std::future::pending().await
    }
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        api().handle(
            Arc::new(App::default()),
            with_budget(post(r#"{"name":"widget"}"#), 150),
            &decode_item,
            &stuck,
        ),
    )
    .await
    .expect("the handler must not hang");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
}

#[tokio::test]
async fn an_expired_invocation_runs_no_step() {
    let app = Arc::new(App::default());
    let response = api()
        .handle(
            Arc::clone(&app),
            with_budget(post(r#"{"name":"widget"}"#), 0),
            &|_: &Request<'_>| -> Result<Item, Failure> { panic!("the decoder must not run") },
            &create,
        )
        .await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(app.calls(), 0);
}

#[tokio::test]
async fn a_finalizer_runs_on_success_and_on_failure_alike() {
    let stamped = || {
        api().finalize(|invocation, response| {
            response
                .headers_mut()
                .insert("cache-control", HeaderValue::from_static("no-store"));
            let id = HeaderValue::from_str(&invocation.request_id).expect("header");
            response.headers_mut().insert("x-request-id", id);
        })
    };
    let success = stamped()
        .handle(
            Arc::new(App::default()),
            with_budget(post(r#"{"name":"widget"}"#), 5_000),
            &decode_item,
            &create,
        )
        .await;
    let failure = stamped()
        .handle(
            Arc::new(App::default()),
            with_budget(post("not json"), 5_000),
            &decode_item,
            &create,
        )
        .await;
    for response in [success, failure] {
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-request-id"], "request-1");
    }
}

#[derive(Debug, Serialize)]
struct Seen {
    request_id: String,
    trace_id: Option<String>,
    invoked_arn: Option<String>,
    remaining_ms: u128,
}

async fn describe(_: Arc<App>, _: (), context: Context<()>) -> Result<Json<Seen>, Failure> {
    let invocation = context.invocation();
    Ok(Json(Seen {
        request_id: invocation.request_id.clone(),
        trace_id: invocation.trace_id.clone(),
        invoked_arn: invocation.invoked_arn.clone(),
        remaining_ms: context.deadline().remaining().as_millis(),
    }))
}

fn seen(response: &HttpResponse) -> serde_json::Value {
    serde_json::from_str(text(response)).expect("JSON")
}

#[tokio::test]
async fn the_handler_sees_the_invocation_the_lambda_context_carries() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let mut context = lambda_runtime::Context::default();
    context.request_id = "request-7".to_owned();
    context.xray_trace_id = Some("Root=1-abc;Parent=def;Sampled=1".to_owned());
    context.invoked_function_arn =
        "arn:aws:lambda:us-east-1:123456789012:function:create-item".to_owned();
    context.deadline = now + 5_000;
    let request = post("").with_lambda_context(context);

    let response = api()
        .handle(
            Arc::new(App::default()),
            request,
            &|_: &Request<'_>| Ok(()),
            &describe,
        )
        .await;
    let seen = seen(&response);
    assert_eq!(seen["request_id"], "request-7");
    assert_eq!(seen["trace_id"], "Root=1-abc;Parent=def;Sampled=1");
    assert_eq!(
        seen["invoked_arn"],
        "arn:aws:lambda:us-east-1:123456789012:function:create-item"
    );
    let remaining = seen["remaining_ms"].as_u64().expect("number");
    assert!(remaining <= 5_000 && remaining > 3_000, "{remaining}");
}

/// A request built by hand has no Lambda context; the pipeline must not
/// panic, and the handler gets Lambda's maximum budget of 15 minutes.
#[tokio::test]
async fn a_request_without_a_lambda_context_gets_an_empty_id_and_the_maximum_budget() {
    let response = api()
        .handle(
            Arc::new(App::default()),
            post(""),
            &|_: &Request<'_>| Ok(()),
            &describe,
        )
        .await;
    let seen = seen(&response);
    assert_eq!(seen["request_id"], "");
    assert!(seen["trace_id"].is_null());
    assert!(seen["invoked_arn"].is_null());
    let remaining = seen["remaining_ms"].as_u64().expect("number");
    assert!(remaining > 14 * 60 * 1_000, "{remaining}");
}

#[test]
fn an_endpoint_reports_its_operation_and_configuration() {
    let api = api().body_limit(64).finalize(|_, _| {});
    assert_eq!(api.operation(), "create-item");
    let debug = format!("{api:?}");
    assert!(debug.contains("create-item"), "{debug}");
    assert!(debug.contains("body_limit: 64"), "{debug}");
    assert!(debug.contains("finalize: true"), "{debug}");
}

#[tokio::test]
async fn a_policy_scope_reaches_the_handler_as_a_checked_value() {
    let response = guarded()
        .handle(
            Arc::new(App::default()),
            post_as(Some("ada"), r#"{"name":"widget"}"#),
            &decode_item,
            &create_owned,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text(&response), r#"{"user":"ada","name":"widget"}"#);
}

#[tokio::test]
async fn a_policy_refusal_is_rendered_by_the_application_renderer() {
    let unknown = guarded()
        .handle(
            Arc::new(App::default()),
            post_as(Some("grace"), r#"{"name":"widget"}"#),
            &decode_item,
            &create_owned,
        )
        .await;
    assert_eq!(unknown.status(), StatusCode::FORBIDDEN);
    assert_eq!(unknown.headers()["content-type"], "application/xml");
    assert_eq!(
        text(&unknown),
        "<error code=\"ERROR_FORBIDDEN\">That user may not do this</error>"
    );

    let anonymous = guarded()
        .handle(
            Arc::new(App::default()),
            post_as(None, r#"{"name":"widget"}"#),
            &decode_item,
            &create_owned,
        )
        .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_decode_failure_is_rendered_by_the_application_renderer() {
    let response = guarded()
        .handle(
            Arc::new(App::default()),
            post_as(Some("ada"), "not json"),
            &decode_item,
            &create_owned,
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        text(&response),
        "<error code=\"ERROR_MALFORMED_BODY\">Invalid request body</error>"
    );
}

/// The renderer reads `public_message`, the only message accessor a
/// downstream crate has, so a 5xx cannot leak through it.
#[tokio::test]
async fn an_application_renderer_cannot_leak_a_5xx() {
    async fn unreachable_store(
        _: Arc<App>,
        _: Item,
        _: Context<Caller>,
    ) -> Result<Json<Owned>, Failure> {
        Err(Failure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "FAULT_STORE",
            "connection refused to 10.0.4.17:5432",
        ))
    }
    let response = guarded()
        .handle(
            Arc::new(App::default()),
            post_as(Some("ada"), r#"{"name":"widget"}"#),
            &decode_item,
            &unreachable_store,
        )
        .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        text(&response),
        "<error code=\"FAULT_STORE\">InternalServerError</error>"
    );
}

#[cfg(feature = "logs")]
mod logs {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use davidrs::Context;
    use davidrs::http::{Failure, Json, Request, StatusCode};

    use super::{App, Item, api, decode_item, post};

    /// Collects everything the subscriber writes.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn store_down(_: Arc<App>, _: Item, _: Context<()>) -> Result<Json<bool>, Failure> {
        Err(Failure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "FAULT_STORE",
            "connection refused to 10.0.4.17:5432",
        ))
    }

    /// The subscriber is installed for this thread only, so other tests are
    /// unaffected.
    #[tokio::test]
    async fn a_failure_is_logged_with_its_code_but_never_its_message() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        api()
            .handle(
                Arc::new(App::default()),
                post(r#"{"name":"widget"}"#),
                &decode_item,
                &store_down,
            )
            .await;
        api()
            .handle(
                Arc::new(App::default()),
                post("not json"),
                &|request: &Request<'_>| request.json::<Item>(),
                &store_down,
            )
            .await;

        let logs = String::from_utf8(captured.0.lock().expect("buffer").clone()).expect("UTF-8");
        assert!(logs.contains("request failed"), "{logs}");
        assert!(logs.contains("FAULT_STORE"), "{logs}");
        assert!(logs.contains("request refused"), "{logs}");
        assert!(logs.contains("ERROR_MALFORMED_BODY"), "{logs}");
        assert!(!logs.contains("10.0.4.17"), "{logs}");
    }
}
