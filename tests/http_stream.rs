//! The streamed pipeline, [`StreamApi`], driven one invocation at a time
//! through [`StreamApi::handle`], and the pieces it is built from: content
//! negotiation, CORS, JSON and event-stream bodies.
//!
//! The Lambda loop itself, [`StreamApi::run`], is exercised in
//! `tests/lambda_loop.rs`.
#![cfg(feature = "http-stream")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use davidrs::http::stream::{
    self, negotiate, sse_frame, Cors, Representation, StreamApi, StreamRequest, StreamResponse,
    EVENT_STREAM,
};
use davidrs::http::{
    codes, literal, Body, ErrorRenderer, Failure, FailureKind, Method, PlainErrors, Policy, Public,
    Request, StatusCode,
};
use davidrs::{Context, Deadline, Invocation};
use lambda_runtime::LambdaEvent;
use serde_json::{json, Value};

const APP: &str = "https://app.example.com";

/// A Function URL request for `GET /orders?status=open`, due in `budget`.
fn event_due_in(method: &str, headers: Value, budget: Duration) -> LambdaEvent<Value> {
    let payload = json!({
        "version": "2.0",
        "rawPath": "/orders",
        "rawQueryString": "status=open",
        "headers": headers,
        "requestContext": {
            "http": { "method": method, "path": "/orders", "sourceIp": "203.0.113.9" },
            "requestId": "r1",
            "stage": "$default",
            "timeEpoch": 0
        },
        "isBase64Encoded": false
    });
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let mut context = lambda_runtime::Context::default();
    context.request_id = "req-1".to_owned();
    context.deadline = now + budget.as_millis() as u64;
    LambdaEvent::new(payload, context)
}

fn event(method: &str, headers: Value) -> LambdaEvent<Value> {
    event_due_in(method, headers, Duration::from_secs(30))
}

fn api() -> StreamApi<Public, PlainErrors> {
    StreamApi::new("orders", Public, PlainErrors).cors(Cors::new(vec![APP.to_owned()]))
}

async fn echo(
    _app: Arc<()>,
    request: StreamRequest,
    _context: Context<()>,
) -> Result<StreamResponse, Failure> {
    Ok(stream::json(
        StatusCode::OK,
        &json!({"path": request.path(), "events": request.wants_events()}),
    ))
}

fn status(response: &StreamResponse) -> StatusCode {
    response.metadata_prelude.status_code
}

fn header<'a>(response: &'a StreamResponse, name: &str) -> Option<&'a str> {
    response
        .metadata_prelude
        .headers
        .get(name)
        .map(|value| value.to_str().expect("text header"))
}

async fn body(response: StreamResponse) -> Vec<u8> {
    response
        .stream
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec()
}

async fn text(response: StreamResponse) -> String {
    String::from_utf8(body(response).await).expect("utf-8")
}

/// Allows a caller that names itself in `x-user`; the scope is that name.
struct NamedCaller;

impl Policy for NamedCaller {
    type Scope = String;

    async fn authorize(&self, request: &Request<'_>, _: &Invocation) -> Result<String, Failure> {
        request
            .header("x-user")
            .map(str::to_owned)
            .ok_or_else(|| Failure::new(StatusCode::UNAUTHORIZED, "UNAUTHENTICATED", "Sign in"))
    }
}

async fn whoami(
    _app: Arc<()>,
    _request: StreamRequest,
    context: Context<String>,
) -> Result<StreamResponse, Failure> {
    Ok(stream::json(
        StatusCode::OK,
        &json!({"user": context.scope()}),
    ))
}

async fn sleep_then_answer(
    _app: Arc<()>,
    _request: StreamRequest,
    _context: Context<()>,
    pause: Duration,
) -> Result<StreamResponse, Failure> {
    tokio::time::sleep(pause).await;
    Ok(stream::json(StatusCode::OK, &json!({"done": true})))
}

#[tokio::test]
async fn an_allowed_origin_gets_its_answer_with_vary_and_cors_headers() {
    let response = api()
        .handle(Arc::new(()), event("GET", json!({"origin": APP})), &echo)
        .await;
    assert_eq!(status(&response), StatusCode::OK);
    assert_eq!(header(&response, "content-type"), Some("application/json"));
    assert_eq!(header(&response, "vary"), Some("Origin, Accept"));
    assert_eq!(header(&response, "access-control-allow-origin"), Some(APP));
    assert_eq!(
        header(&response, "access-control-allow-methods"),
        Some("GET, OPTIONS")
    );
    assert_eq!(
        header(&response, "access-control-allow-headers"),
        Some("accept,authorization,content-type")
    );
    assert_eq!(header(&response, "access-control-max-age"), Some("600"));
    assert_eq!(header(&response, "access-control-expose-headers"), None);
    assert_eq!(text(response).await, r#"{"path":"/orders","events":false}"#);
}

#[tokio::test]
async fn an_origin_outside_the_allowlist_is_refused_before_the_handler() {
    let response = api()
        .handle(
            Arc::new(()),
            event("GET", json!({"origin": "https://other.example.com"})),
            &echo,
        )
        .await;
    assert_eq!(status(&response), StatusCode::FORBIDDEN);
    assert_eq!(header(&response, "vary"), Some("Origin, Accept"));
    assert_eq!(header(&response, "access-control-allow-origin"), None);
    assert!(text(response).await.contains(codes::ORIGIN_NOT_ALLOWED));
}

#[tokio::test]
async fn a_request_without_an_origin_is_served_without_cors_headers() {
    let response = api()
        .handle(Arc::new(()), event("GET", json!({})), &echo)
        .await;
    assert_eq!(status(&response), StatusCode::OK);
    assert_eq!(header(&response, "vary"), Some("Origin, Accept"));
    assert_eq!(header(&response, "access-control-allow-origin"), None);
    assert_eq!(header(&response, "access-control-max-age"), None);
}

#[tokio::test]
async fn an_endpoint_without_cors_neither_refuses_nor_answers_an_origin() {
    let response = StreamApi::new("orders", Public, PlainErrors)
        .handle(
            Arc::new(()),
            event("GET", json!({"origin": "https://other.example.com"})),
            &echo,
        )
        .await;
    assert_eq!(status(&response), StatusCode::OK);
    assert_eq!(header(&response, "access-control-allow-origin"), None);
}

#[tokio::test]
async fn a_preflight_is_a_204_carrying_the_cors_policy() {
    let response = api()
        .handle(
            Arc::new(()),
            event("OPTIONS", json!({"origin": APP})),
            &echo,
        )
        .await;
    assert_eq!(status(&response), StatusCode::NO_CONTENT);
    assert_eq!(header(&response, "access-control-allow-origin"), Some(APP));
    assert_eq!(
        header(&response, "access-control-allow-methods"),
        Some("GET, OPTIONS")
    );
    assert!(body(response).await.is_empty());
}

#[tokio::test]
async fn cors_settings_reach_the_response_headers() {
    let cors = Cors::new(vec![APP.to_owned()])
        .allow_headers("x-client")
        .expose_headers("x-request-id")
        .max_age(Duration::from_millis(60_500));
    let response = StreamApi::new("orders", Public, PlainErrors)
        .cors(cors)
        .handle(
            Arc::new(()),
            event("OPTIONS", json!({"origin": APP})),
            &echo,
        )
        .await;
    assert_eq!(
        header(&response, "access-control-allow-headers"),
        Some("x-client")
    );
    assert_eq!(
        header(&response, "access-control-expose-headers"),
        Some("x-request-id")
    );
    assert_eq!(header(&response, "access-control-max-age"), Some("60"));
}

#[test]
fn cors_allows_only_the_exact_origins_it_was_given() {
    let cors = Cors::new(vec![APP.to_owned()]);
    assert!(cors.allows(APP));
    assert!(!cors.allows("http://app.example.com"));
    assert!(!cors.allows("https://app.example.com:8443"));
    assert!(!cors.allows("https://evil.app.example.com"));
    assert!(!cors.allows(""));
}

#[tokio::test]
async fn a_method_the_endpoint_does_not_serve_is_a_405_with_allow() {
    let response = api()
        .handle(Arc::new(()), event("POST", json!({})), &echo)
        .await;
    assert_eq!(status(&response), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(header(&response, "allow"), Some("GET, OPTIONS"));
    let text = text(response).await;
    assert!(text.contains(codes::METHOD_NOT_ALLOWED));
    assert!(text.contains("Use GET"));
}

#[tokio::test]
async fn configured_methods_are_served_and_announced() {
    let api = api().methods(&[Method::GET, Method::POST]);
    let post = api
        .handle(Arc::new(()), event("POST", json!({})), &echo)
        .await;
    assert_eq!(status(&post), StatusCode::OK);

    let delete = api
        .handle(Arc::new(()), event("DELETE", json!({})), &echo)
        .await;
    assert_eq!(status(&delete), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(header(&delete, "allow"), Some("GET, POST, OPTIONS"));
    assert!(text(delete).await.contains("Use GET or POST"));
}

#[tokio::test]
async fn a_payload_that_is_not_an_http_request_is_a_400() {
    let mut event = event("GET", json!({"origin": APP}));
    event.payload = json!(["not", "a", "request"]);
    let response = api().handle(Arc::new(()), event, &echo).await;
    assert_eq!(status(&response), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, "vary"), Some("Origin, Accept"));
    assert_eq!(header(&response, "access-control-allow-origin"), None);
    assert!(text(response).await.contains(codes::MALFORMED_REQUEST));
}

#[tokio::test]
async fn prepare_runs_before_anything_reads_the_request() {
    let api = api().prepare(|request| {
        if let Some(origin) = request.headers_mut().remove("x-relocated-origin") {
            request.headers_mut().insert("origin", origin);
        }
    });
    let response = api
        .handle(
            Arc::new(()),
            event("GET", json!({"x-relocated-origin": APP})),
            &echo,
        )
        .await;
    assert_eq!(status(&response), StatusCode::OK);
    assert_eq!(header(&response, "access-control-allow-origin"), Some(APP));
}

#[tokio::test]
async fn the_finalizer_runs_on_success_and_failure_alike() {
    let api = api().finalize(|invocation, headers| {
        let id = invocation.request_id.parse().expect("header value");
        headers.insert("x-request-id", id);
    });
    let success = api
        .handle(Arc::new(()), event("GET", json!({})), &echo)
        .await;
    let failure = api
        .handle(Arc::new(()), event("POST", json!({})), &echo)
        .await;
    assert_eq!(status(&success), StatusCode::OK);
    assert_eq!(header(&success, "x-request-id"), Some("req-1"));
    assert_eq!(status(&failure), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(header(&failure, "x-request-id"), Some("req-1"));
}

#[tokio::test]
async fn a_policy_refusal_is_rendered_with_cors_and_vary() {
    let response = StreamApi::new("orders", NamedCaller, PlainErrors)
        .cors(Cors::new(vec![APP.to_owned()]))
        .handle(Arc::new(()), event("GET", json!({"origin": APP})), &whoami)
        .await;
    assert_eq!(status(&response), StatusCode::UNAUTHORIZED);
    assert_eq!(header(&response, "vary"), Some("Origin, Accept"));
    assert_eq!(header(&response, "access-control-allow-origin"), Some(APP));
    assert!(text(response).await.contains("UNAUTHENTICATED"));
}

#[tokio::test]
async fn the_policy_scope_reaches_the_handler() {
    let response = StreamApi::new("orders", NamedCaller, PlainErrors)
        .handle(
            Arc::new(()),
            event("GET", json!({"x-user": "ada"})),
            &whoami,
        )
        .await;
    assert_eq!(status(&response), StatusCode::OK);
    assert_eq!(text(response).await, r#"{"user":"ada"}"#);
}

#[tokio::test]
async fn an_unreadable_accept_header_is_refused_before_the_policy() {
    let response = StreamApi::new("orders", NamedCaller, PlainErrors)
        .handle(
            Arc::new(()),
            event("GET", json!({"accept": "application/json;q=high"})),
            &whoami,
        )
        .await;
    assert_eq!(status(&response), StatusCode::BAD_REQUEST);
    assert!(text(response).await.contains(codes::INVALID_ACCEPT));
}

/// With a 1.5 s invocation and the default one-second margin, the handler
/// has about half a second.
#[tokio::test]
async fn the_default_margin_keeps_a_second_back_and_a_late_handler_is_a_504() {
    let started = Instant::now();
    let response = api()
        .handle(
            Arc::new(()),
            event_due_in("GET", json!({}), Duration::from_millis(1_500)),
            &|app, request, context| {
                sleep_then_answer(app, request, context, Duration::from_secs(60))
            },
        )
        .await;
    assert!(started.elapsed() < Duration::from_millis(1_300));
    assert_eq!(status(&response), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(header(&response, "vary"), Some("Origin, Accept"));
    assert!(text(response).await.contains(codes::TIMEOUT));
}

#[tokio::test]
async fn a_smaller_margin_gives_the_handler_that_time() {
    let response = api()
        .margin(Duration::from_millis(100))
        .handle(
            Arc::new(()),
            event_due_in("GET", json!({}), Duration::from_millis(1_500)),
            &|app, request, context| {
                sleep_then_answer(app, request, context, Duration::from_millis(800))
            },
        )
        .await;
    assert_eq!(status(&response), StatusCode::OK);
}

#[tokio::test]
async fn the_handler_sees_the_request_the_payload_and_the_representation() {
    async fn describe(
        _app: Arc<()>,
        request: StreamRequest,
        _context: Context<()>,
    ) -> Result<StreamResponse, Failure> {
        Ok(stream::json(
            StatusCode::OK,
            &json!({
                "method": request.native().method().as_str(),
                "query": request.view().query_string(),
                "user": request.view().header("x-user"),
                "path": request.path(),
                "raw": request.event()["rawQueryString"],
                "json": request.representation() == Representation::Json,
            }),
        ))
    }
    let response = api()
        .handle(
            Arc::new(()),
            event(
                "GET",
                json!({"x-user": "ada", "accept": "application/json"}),
            ),
            &describe,
        )
        .await;
    let described: Value = serde_json::from_slice(&body(response).await).expect("json");
    assert_eq!(
        described,
        json!({
            "method": "GET",
            "query": "status=open",
            "user": "ada",
            "path": "/orders",
            "raw": "status=open",
            "json": true,
        })
    );
}

#[tokio::test]
async fn an_event_stream_is_negotiated_and_streamed_frame_by_frame() {
    async fn count(
        _app: Arc<()>,
        request: StreamRequest,
        context: Context<()>,
    ) -> Result<StreamResponse, Failure> {
        assert_eq!(request.representation(), Representation::EventStream);
        Ok(stream::events(context.deadline(), |writer| async move {
            for n in 1..=3 {
                if writer.producer().should_stop() {
                    return;
                }
                let frame = sse_frame(Some(&n.to_string()), Some("count"), &n.to_string());
                if writer.send(frame).await.is_err() {
                    return;
                }
            }
        }))
    }
    let response = api()
        .handle(
            Arc::new(()),
            event("GET", json!({"accept": "text/event-stream"})),
            &count,
        )
        .await;
    assert_eq!(status(&response), StatusCode::OK);
    assert_eq!(header(&response, "content-type"), Some(EVENT_STREAM));
    assert_eq!(
        text(response).await,
        "id: 1\nevent: count\ndata: 1\n\n\
         id: 2\nevent: count\ndata: 2\n\n\
         id: 3\nevent: count\ndata: 3\n\n"
    );
}

/// The event stream format ends a line at `\r` too: left in place, the text
/// after it would be read as a separate, unknown field and dropped.
#[test]
fn every_kind_of_line_break_in_data_starts_a_new_data_line() {
    assert_eq!(
        sse_frame(None, None, "a\rb\r\nc\nd"),
        "data: a\ndata: b\ndata: c\ndata: d\n\n"
    );
    assert_eq!(sse_frame(None, None, "a\n"), "data: a\ndata: \n\n");
}

/// An id or event name taken from a request could otherwise end its line and
/// write a field of its own.
#[test]
fn line_breaks_in_an_id_or_event_cannot_add_a_field() {
    assert_eq!(
        sse_frame(Some("7\nevent: admin\r\n"), Some("tick\r\ndata: x"), "{}"),
        "id: 7event: admin\nevent: tickdata: x\ndata: {}\n\n"
    );
}

#[test]
fn an_sse_frame_splits_lines_and_omits_absent_fields() {
    assert_eq!(sse_frame(None, None, "a\nb"), "data: a\ndata: b\n\n");
    assert_eq!(sse_frame(Some("7"), None, ""), "id: 7\ndata: \n\n");
    assert_eq!(
        sse_frame(None, Some("done"), "{}"),
        "event: done\ndata: {}\n\n"
    );
}

fn a_minute() -> Deadline {
    Deadline::in_from_now(Duration::from_secs(60))
}

#[tokio::test]
async fn a_writer_fails_once_the_reader_is_gone() {
    let (report, outcome) = tokio::sync::oneshot::channel();
    let response = stream::events(a_minute(), |writer| async move {
        let error = loop {
            if let Err(error) = writer.send(sse_frame(None, None, "tick")).await {
                break error;
            }
        };
        let _ = report.send(error.to_string());
    });
    drop(response);
    let message = tokio::time::timeout(Duration::from_secs(5), outcome)
        .await
        .expect("the producer reports in time")
        .expect("the producer reports");
    assert_eq!(message, "the event stream was closed");
}

/// Nobody reads the response: four frames fill the buffer and the fifth
/// waits until the one-second send limit gives up.
#[tokio::test]
async fn a_writer_fails_when_the_reader_stalls_for_a_second() {
    let (report, outcome) = tokio::sync::oneshot::channel();
    let started = Instant::now();
    let response = stream::events(a_minute(), |writer| async move {
        let error = loop {
            if let Err(error) = writer.send(sse_frame(None, None, "tick")).await {
                break error;
            }
        };
        let _ = report.send(error.to_string());
    });
    let message = tokio::time::timeout(Duration::from_secs(5), outcome)
        .await
        .expect("the producer reports in time")
        .expect("the producer reports");
    assert_eq!(message, "the event stream reader stalled");
    assert!(started.elapsed() >= Duration::from_millis(900));
    drop(response);
}

#[tokio::test]
async fn a_204_json_response_has_no_body() {
    let response = stream::json(StatusCode::NO_CONTENT, &json!({"ignored": true}));
    assert_eq!(status(&response), StatusCode::NO_CONTENT);
    assert_eq!(header(&response, "content-type"), Some("application/json"));
    assert!(body(response).await.is_empty());
}

#[tokio::test]
async fn a_rendered_failure_keeps_its_status_headers_and_body() {
    let failure = Failure::new(StatusCode::TOO_MANY_REQUESTS, "SLOW_DOWN", "Slow down")
        .with_header("retry-after", "5");
    let response = stream::from_response(PlainErrors.render(&failure));
    assert_eq!(status(&response), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&response, "retry-after"), Some("5"));
    assert!(text(response).await.contains("SLOW_DOWN"));
}

#[tokio::test]
async fn empty_text_and_binary_bodies_stream_unchanged() {
    let empty = lambda_http::Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .body(Body::Empty)
        .expect("response");
    let binary = lambda_http::Response::builder()
        .body(Body::Binary(vec![0, 159, 255]))
        .expect("response");
    let text_response = literal(StatusCode::OK, "text/plain", "plain".to_owned());

    let empty = stream::from_response(empty);
    assert_eq!(status(&empty), StatusCode::NOT_MODIFIED);
    assert!(body(empty).await.is_empty());
    assert_eq!(body(stream::from_response(binary)).await, [0, 159, 255]);
    let text_response = stream::from_response(text_response);
    assert_eq!(header(&text_response, "content-type"), Some("text/plain"));
    assert_eq!(text(text_response).await, "plain");
}

#[test]
fn the_endpoint_names_its_operation_for_telemetry() {
    let api = api().margin(Duration::from_millis(250));
    assert_eq!(api.operation(), "orders");
    let debug = format!("{api:?}");
    assert!(debug.starts_with("StreamApi"));
    assert!(debug.contains("\"orders\""));
    assert!(debug.contains("250ms"));
}

#[test]
fn negotiation_defaults_to_json() {
    for accept in [None, Some("*/*"), Some("application/json")] {
        assert_eq!(negotiate(accept).expect("acceptable"), Representation::Json);
    }
}

#[test]
fn the_higher_quality_wins_and_json_wins_a_tie() {
    let cases = [
        (
            "application/json;q=0.5, text/event-stream",
            Representation::EventStream,
        ),
        (
            "application/json;q=0.5, text/event-stream;q=0.5",
            Representation::Json,
        ),
        ("*/*;q=0.5, text/event-stream;q=0.4", Representation::Json),
        (
            "text/event-stream;q=0.2, text/event-stream;q=0.8, application/json;q=0.5",
            Representation::EventStream,
        ),
    ];
    for (accept, expected) in cases {
        assert_eq!(negotiate(Some(accept)).expect(accept), expected, "{accept}");
    }
}

/// A specific range sets the quality even when a broader one is higher.
#[test]
fn the_most_specific_matching_range_sets_the_quality() {
    let cases = [
        ("text/event-stream;q=0, */*", Representation::Json),
        (
            "text/event-stream;q=0.2, text/*;q=0.9, application/json;q=0.5",
            Representation::Json,
        ),
        (
            "text/*;q=0.2, text/event-stream;q=0.9, application/json;q=0.5",
            Representation::EventStream,
        ),
    ];
    for (accept, expected) in cases {
        assert_eq!(negotiate(Some(accept)).expect(accept), expected, "{accept}");
    }
    assert_eq!(
        negotiate(Some("text/event-stream;q=0, text/*"))
            .expect_err("both excluded")
            .code(),
        codes::NOT_ACCEPTABLE
    );
}

#[test]
fn a_type_group_matches_its_representation() {
    assert_eq!(
        negotiate(Some("application/*")).expect("json"),
        Representation::Json
    );
    assert_eq!(
        negotiate(Some("text/*")).expect("stream"),
        Representation::EventStream
    );
    assert_eq!(
        negotiate(Some("text/*, application/*;q=0.9")).expect("stream"),
        Representation::EventStream
    );
}

#[test]
fn media_types_and_the_quality_name_are_case_insensitive() {
    assert_eq!(
        negotiate(Some("TEXT/Event-Stream")).expect("stream"),
        Representation::EventStream
    );
    assert_eq!(
        negotiate(Some("Application/JSON;Q=0.1, text/event-stream;q=0.5")).expect("stream"),
        Representation::EventStream
    );
}

#[test]
fn parameters_other_than_quality_are_ignored() {
    assert_eq!(
        negotiate(Some("application/json; charset=utf-8")).expect("json"),
        Representation::Json
    );
    assert_eq!(
        negotiate(Some(
            "text/event-stream; charset=utf-8; q=0.5, application/json;q=0.4"
        ))
        .expect("stream"),
        Representation::EventStream
    );
}

#[test]
fn excluding_every_representation_is_a_406() {
    for accept in [
        "text/html",
        "application/json;q=0",
        "application/json;q=0, text/event-stream;q=0",
    ] {
        let failure = negotiate(Some(accept)).expect_err(accept);
        assert_eq!(failure.status(), StatusCode::NOT_ACCEPTABLE, "{accept}");
        assert_eq!(failure.code(), codes::NOT_ACCEPTABLE);
        assert_eq!(failure.kind(), FailureKind::Decode);
    }
}

#[test]
fn an_unreadable_quality_is_a_400() {
    for accept in [
        "application/json;q=high",
        "application/json;q=2",
        "application/json;q=-1",
        "application/json;q=NaN",
        "application/json;q=inf",
    ] {
        let failure = negotiate(Some(accept)).expect_err(accept);
        assert_eq!(failure.status(), StatusCode::BAD_REQUEST, "{accept}");
        assert_eq!(failure.code(), codes::INVALID_ACCEPT);
        assert_eq!(failure.kind(), FailureKind::Decode);
    }
}

#[tokio::test]
async fn a_handler_is_not_called_after_a_refusal() {
    let called = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&called);
    let handler = move |_app: Arc<()>, _request: StreamRequest, _context: Context<()>| {
        flag.store(true, Ordering::SeqCst);
        async { Ok(stream::json(StatusCode::OK, &Value::Null)) }
    };
    for refused in [
        event("GET", json!({"origin": "https://other.example.com"})),
        event("PUT", json!({})),
        event("GET", json!({"accept": "text/html"})),
    ] {
        let response = api().handle(Arc::new(()), refused, &handler).await;
        assert!(status(&response).is_client_error());
    }
    assert!(!called.load(Ordering::SeqCst));
}
