//! Every `run` entry point, driven through one real invocation.
//!
//! `RuntimeApi` plays the Lambda Runtime API on a local port. It answers the
//! first `GET /2018-06-01/runtime/invocation/next` with one event and the
//! headers Lambda sends, holds every later `/next` open, and hands the test
//! the first response or error the loop posts back; the test then drops the
//! loop. The loop reads its endpoint and function configuration from process
//! environment variables, so a `RuntimeApi` holds one lock for its lifetime
//! and these tests run one at a time.
#![cfg(feature = "runtime")]

mod support;

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use davidrs::{error_chain, Context, RuntimeError};
use serde_json::{json, Value};
use support::server::{reply, Recorded, Reply, Server};
use tokio::sync::{mpsc, Mutex, MutexGuard};

const REQUEST_ID: &str = "8476a536-e9f4-11e8-9739-2dfe598c3fcd";
const FUNCTION_ARN: &str = "arn:aws:lambda:us-east-1:123456789012:function:example";
const TRACE_ID: &str = "Root=1-5759e988-bd862e3fe1be46a994272793;Parent=53995c3f42cd8ad8;Sampled=1";
/// The tenant a tenant-isolated function is invoked for.
const TENANT_ID: &str = "tenant-7";

/// How long any one step may take before the test fails instead of hanging.
const PATIENCE: Duration = Duration::from_secs(10);

/// Held by each `RuntimeApi`: the loop reads the process environment.
static ENVIRONMENT: Mutex<()> = Mutex::const_new(());

/// A local Runtime API that serves one invocation.
struct RuntimeApi {
    _server: Server,
    posted: mpsc::UnboundedReceiver<Recorded>,
    _environment: MutexGuard<'static, ()>,
}

impl RuntimeApi {
    /// Serves `event` once, with a deadline 30 seconds from now.
    async fn serve(event: Value) -> Self {
        Self::serve_with_deadline(event, epoch_ms_in(Duration::from_secs(30))).await
    }

    /// Serves `event` once, announcing `deadline_ms` as its deadline, and
    /// points the loop's environment at this server.
    async fn serve_with_deadline(event: Value, deadline_ms: String) -> Self {
        let environment = ENVIRONMENT.lock().await;
        let (sender, posted) = mpsc::unbounded_channel();
        let delivered = Arc::new(AtomicBool::new(false));
        let server = Server::start(move |request: Recorded| {
            let next = request.path == "/2018-06-01/runtime/invocation/next";
            let hold = next && delivered.swap(true, Ordering::SeqCst);
            let sender = sender.clone();
            let event = event.to_string();
            let deadline_ms = deadline_ms.clone();
            async move {
                if hold {
                    std::future::pending::<()>().await;
                }
                if next {
                    return invocation(event, &deadline_ms);
                }
                let _ = sender.send(request);
                reply(202, "application/json", r#"{"status":"OK"}"#)
            }
        })
        .await;
        let variables = [
            ("AWS_LAMBDA_RUNTIME_API", server.authority()),
            ("AWS_LAMBDA_FUNCTION_NAME", "example".to_owned()),
            ("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128".to_owned()),
            ("AWS_LAMBDA_FUNCTION_VERSION", "$LATEST".to_owned()),
            (
                "AWS_LAMBDA_LOG_GROUP_NAME",
                "/aws/lambda/example".to_owned(),
            ),
            (
                "AWS_LAMBDA_LOG_STREAM_NAME",
                "2026/01/01/[$LATEST]0f1e2d3c".to_owned(),
            ),
        ];
        for (name, value) in variables {
            std::env::set_var(name, value);
        }
        Self {
            _server: server,
            posted,
            _environment: environment,
        }
    }

    /// A Runtime API whose `/next` answer the loop cannot read: its deadline
    /// is not a number.
    async fn broken() -> Self {
        Self::serve_with_deadline(json!({}), "soon".to_owned()).await
    }

    /// Runs `lambda` until it posts its first answer, then drops it and
    /// returns what was posted.
    async fn answer(mut self, lambda: impl Future<Output = Result<(), RuntimeError>>) -> Recorded {
        let posted = self.posted.recv();
        let answered = tokio::time::timeout(PATIENCE, async {
            tokio::select! {
                ended = lambda => panic!("the loop ended before answering: {ended:?}"),
                posted = posted => posted.expect("the server is running"),
            }
        });
        answered.await.expect("the loop answered in time")
    }

    /// Runs `lambda` to its end and returns the error it ended with.
    async fn failure(self, lambda: impl Future<Output = Result<(), RuntimeError>>) -> RuntimeError {
        tokio::time::timeout(PATIENCE, lambda)
            .await
            .expect("the loop ended in time")
            .expect_err("a broken Runtime API ends the loop")
    }
}

/// The `/next` answer: the event and the headers Lambda sends with it.
fn invocation(event: String, deadline_ms: &str) -> Reply {
    let mut response = reply(200, "application/json", event);
    let headers = [
        ("lambda-runtime-aws-request-id", REQUEST_ID),
        ("lambda-runtime-deadline-ms", deadline_ms),
        ("lambda-runtime-invoked-function-arn", FUNCTION_ARN),
        ("lambda-runtime-trace-id", TRACE_ID),
        ("lambda-runtime-aws-tenant-id", TENANT_ID),
    ];
    for (name, value) in headers {
        let value = hyper::header::HeaderValue::from_str(value).expect("header value");
        response.headers_mut().insert(name, value);
    }
    response
}

/// Epoch milliseconds `budget` from now, as Lambda sends a deadline.
fn epoch_ms_in(budget: Duration) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970");
    (now + budget).as_millis().to_string()
}

fn response_path() -> String {
    format!("/2018-06-01/runtime/invocation/{REQUEST_ID}/response")
}

fn error_path() -> String {
    format!("/2018-06-01/runtime/invocation/{REQUEST_ID}/error")
}

fn json_body(posted: &Recorded) -> Value {
    serde_json::from_slice(&posted.body).expect("a JSON body")
}

/// Asserts that `error` is the loop's own failure, with its cause.
fn assert_loop_failure(error: &RuntimeError) {
    assert_eq!(error.to_string(), "lambda runtime");
    assert_eq!(
        error_chain(error),
        "lambda runtime: invalid digit found in string"
    );
}

/// Splits a streamed answer into its metadata prelude and its body.
///
/// A streamed response is posted as a JSON prelude (status, headers,
/// cookies), eight NUL bytes, then the stream itself.
#[cfg(feature = "streaming")]
fn streamed(posted: &Recorded) -> (Value, String) {
    assert_eq!(posted.path, response_path());
    assert_eq!(
        posted.header("lambda-runtime-function-response-mode"),
        Some("streaming")
    );
    let separator = posted
        .body
        .windows(8)
        .position(|window| window == [0; 8])
        .expect("eight NUL bytes after the prelude");
    let prelude = serde_json::from_slice(&posted.body[..separator]).expect("a JSON prelude");
    let body = std::str::from_utf8(&posted.body[separator + 8..]).expect("a UTF-8 stream");
    (prelude, body.to_owned())
}

/// Every entry point returns the loop's own failure as a [`RuntimeError`]
/// instead of panicking or hanging.
#[tokio::test]
async fn every_entry_point_returns_a_runtime_error_when_the_runtime_api_breaks() {
    let lambda = davidrs::runtime::run(Arc::new(()), direct::price);
    assert_loop_failure(&RuntimeApi::broken().await.failure(lambda).await);
    #[cfg(feature = "event")]
    {
        let lambda = davidrs::event::run(Arc::default(), event::remember);
        assert_loop_failure(&RuntimeApi::broken().await.failure(lambda).await);
    }
    #[cfg(feature = "schedule")]
    {
        let lambda = davidrs::schedule::run(Arc::default(), schedule::purge);
        assert_loop_failure(&RuntimeApi::broken().await.failure(lambda).await);
    }
    #[cfg(feature = "queue")]
    {
        let lambda = davidrs::queue::run(Arc::new(()), queue::consume);
        assert_loop_failure(&RuntimeApi::broken().await.failure(lambda).await);
    }
    #[cfg(feature = "streaming")]
    {
        let lambda = davidrs::streaming::run(Arc::new(()), streaming::count);
        assert_loop_failure(&RuntimeApi::broken().await.failure(lambda).await);
    }
    #[cfg(feature = "http")]
    {
        let lambda = http::api().run(Arc::new(()), http::decode, http::find);
        assert_loop_failure(&RuntimeApi::broken().await.failure(lambda).await);
    }
    #[cfg(feature = "http-stream")]
    {
        let lambda = stream::api().run(Arc::new(()), stream::ticks);
        assert_loop_failure(&RuntimeApi::broken().await.failure(lambda).await);
    }
}

/// `runtime::run`: any typed payload in, any serializable answer out.
mod direct {
    use super::*;

    #[derive(serde::Deserialize)]
    pub(super) struct Order {
        order_id: String,
        quantity: u32,
    }

    #[derive(serde::Serialize)]
    pub(super) struct Receipt {
        order_id: String,
        total: u32,
    }

    pub(super) async fn price(
        _: Arc<()>,
        order: Order,
        _: Context<()>,
    ) -> Result<Receipt, RuntimeError> {
        Ok(Receipt {
            order_id: order.order_id,
            total: order.quantity * 10,
        })
    }

    async fn describe(_: Arc<()>, _: Value, context: Context<()>) -> Result<Value, RuntimeError> {
        let invocation = context.invocation();
        Ok(json!({
            "request_id": invocation.request_id,
            "trace_id": invocation.trace_id,
            "invoked_arn": invocation.invoked_arn,
            "tenant_id": invocation.tenant_id,
            "remaining_ms": context.deadline().remaining().as_millis() as u64,
        }))
    }

    async fn refuse(_: Arc<()>, _: Value, _: Context<()>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::message("quantity must be positive"))
    }

    async fn stall(_: Arc<()>, _: Value, _: Context<()>) -> Result<Value, RuntimeError> {
        std::future::pending::<()>().await;
        Ok(Value::Null)
    }

    #[tokio::test]
    async fn the_handler_output_is_posted_as_the_response() {
        let lambda = davidrs::runtime::run(Arc::new(()), price);
        let event = json!({"order_id": "o-1", "quantity": 3});
        let posted = RuntimeApi::serve(event).await.answer(lambda).await;
        assert_eq!(posted.method, "POST");
        assert_eq!(posted.path, response_path());
        assert_eq!(json_body(&posted), json!({"order_id": "o-1", "total": 30}));
    }

    #[tokio::test]
    async fn the_invocation_headers_reach_the_handler() {
        let lambda = davidrs::runtime::run(Arc::new(()), describe);
        let seen = json_body(&RuntimeApi::serve(json!({})).await.answer(lambda).await);
        assert_eq!(seen["request_id"], REQUEST_ID);
        assert_eq!(seen["trace_id"], TRACE_ID);
        assert_eq!(seen["invoked_arn"], FUNCTION_ARN);
        assert_eq!(seen["tenant_id"], TENANT_ID);
        let remaining = seen["remaining_ms"].as_u64().expect("a number");
        assert!(
            (20_000..=30_000).contains(&remaining),
            "{remaining} ms left of 30 s"
        );
    }

    #[tokio::test]
    async fn a_handler_error_is_posted_to_error_with_its_message() {
        let lambda = davidrs::runtime::run(Arc::new(()), refuse);
        let posted = RuntimeApi::serve(json!({})).await.answer(lambda).await;
        assert_eq!(posted.path, error_path());
        assert_eq!(
            posted.header("lambda-runtime-function-error-type"),
            Some("unhandled")
        );
        assert_eq!(
            json_body(&posted)["errorMessage"],
            "quantity must be positive"
        );
    }

    #[tokio::test]
    async fn a_handler_that_overruns_its_budget_is_posted_as_a_deadline_error() {
        let lambda = davidrs::runtime::run(Arc::new(()), stall);
        let deadline = epoch_ms_in(Duration::from_millis(300));
        let api = RuntimeApi::serve_with_deadline(json!({}), deadline).await;
        let posted = api.answer(lambda).await;
        assert_eq!(posted.path, error_path());
        let message = json_body(&posted)["errorMessage"].to_string();
        assert!(message.contains("deadline exceeded after"), "{message}");
    }
}

#[cfg(feature = "event")]
mod event {
    use std::sync::Mutex as Seen;

    use davidrs::event::Event;

    use super::*;

    #[derive(serde::Deserialize)]
    pub(super) struct OrderCreated {
        order_id: String,
    }

    pub(super) async fn remember(
        seen: Arc<Seen<Vec<String>>>,
        event: Event<OrderCreated>,
        _: Context<()>,
    ) -> Result<(), RuntimeError> {
        let summary = format!(
            "{} {} {}",
            event.id, event.detail_type, event.detail.order_id
        );
        seen.lock().expect("lock").push(summary);
        Ok(())
    }

    #[tokio::test]
    async fn the_handler_gets_a_typed_event_and_null_is_posted() {
        let seen = Arc::new(Seen::new(Vec::new()));
        let lambda = davidrs::event::run(Arc::clone(&seen), remember);
        let event = json!({
            "version": "0",
            "id": "e-1",
            "detail-type": "order.created",
            "source": "shop.orders",
            "account": "123456789012",
            "time": "2026-01-01T00:00:00Z",
            "region": "us-east-1",
            "resources": [],
            "detail": { "order_id": "o-1" }
        });
        let posted = RuntimeApi::serve(event).await.answer(lambda).await;
        assert_eq!(posted.path, response_path());
        assert_eq!(json_body(&posted), Value::Null);
        assert_eq!(*seen.lock().expect("lock"), ["e-1 order.created o-1"]);
    }
}

#[cfg(feature = "schedule")]
mod schedule {
    use std::sync::Mutex as Seen;

    use super::*;

    #[derive(serde::Deserialize)]
    pub(super) struct Purge {
        older_than_days: u32,
    }

    pub(super) async fn purge(
        seen: Arc<Seen<Vec<u32>>>,
        input: Purge,
        _: Context<()>,
    ) -> Result<(), RuntimeError> {
        seen.lock().expect("lock").push(input.older_than_days);
        Ok(())
    }

    #[tokio::test]
    async fn the_handler_gets_the_typed_payload_and_null_is_posted() {
        let seen = Arc::new(Seen::new(Vec::new()));
        let lambda = davidrs::schedule::run(Arc::clone(&seen), purge);
        let event = json!({"older_than_days": 7});
        let posted = RuntimeApi::serve(event).await.answer(lambda).await;
        assert_eq!(posted.path, response_path());
        assert_eq!(json_body(&posted), Value::Null);
        assert_eq!(*seen.lock().expect("lock"), [7]);
    }

    /// The reason the adapter exists: a payload that stopped matching its
    /// type fails loudly, and the handler never runs.
    #[tokio::test]
    async fn a_payload_that_does_not_match_the_type_is_posted_as_an_error() {
        let seen = Arc::new(Seen::new(Vec::new()));
        let lambda = davidrs::schedule::run(Arc::clone(&seen), purge);
        let event = json!({"older_than_days": "seven"});
        let posted = RuntimeApi::serve(event).await.answer(lambda).await;
        assert_eq!(posted.path, error_path());
        assert!(seen.lock().expect("lock").is_empty());
    }
}

#[cfg(feature = "queue")]
mod queue {
    use davidrs::queue::{Delivery, Disposition};

    use super::*;

    pub(super) async fn consume(
        _: Arc<()>,
        delivery: Delivery,
        _: Context<()>,
    ) -> Result<Disposition, RuntimeError> {
        match delivery.body.as_str() {
            "done" => Ok(Disposition::Delete),
            "later" => Ok(Disposition::Retry),
            _ => Err(RuntimeError::message("unreadable message")),
        }
    }

    fn record(message_id: &str, body: &str) -> Value {
        json!({
            "messageId": message_id,
            "receiptHandle": format!("handle-{message_id}"),
            "body": body,
            "attributes": { "ApproximateReceiveCount": "1" },
            "messageAttributes": {},
            "md5OfBody": "d41d8cd98f00b204e9800998ecf8427e",
            "eventSource": "aws:sqs",
            "eventSourceARN": "arn:aws:sqs:us-east-1:123456789012:orders",
            "awsRegion": "us-east-1"
        })
    }

    #[tokio::test]
    async fn the_records_to_redeliver_are_posted_as_batch_item_failures() {
        let lambda = davidrs::queue::run(Arc::new(()), consume);
        let event = json!({
            "Records": [record("m-1", "done"), record("m-2", "later"), record("m-3", "{")]
        });
        let posted = RuntimeApi::serve(event).await.answer(lambda).await;
        assert_eq!(posted.path, response_path());
        assert_eq!(
            json_body(&posted),
            json!({"batchItemFailures": [{"itemIdentifier": "m-2"}, {"itemIdentifier": "m-3"}]})
        );
    }
}

#[cfg(feature = "streaming")]
mod streaming {
    use davidrs::streaming::StreamBody;
    use hyper::header::HeaderValue;
    use hyper::StatusCode;
    use lambda_runtime::MetadataPrelude;

    use super::*;

    pub(super) async fn count(
        _: Arc<()>,
        _: Value,
        context: Context<()>,
    ) -> (MetadataPrelude, StreamBody) {
        let mut prelude = MetadataPrelude {
            status_code: StatusCode::CREATED,
            ..MetadataPrelude::default()
        };
        prelude
            .headers
            .insert("content-type", HeaderValue::from_static("text/plain"));
        let body = StreamBody::spawn(1, context.deadline(), |producer| async move {
            for n in 1..=3 {
                if !producer.send(format!("{n}\n")).await {
                    return;
                }
            }
        });
        (prelude, body)
    }

    async fn stall(_: Arc<()>, _: Value, _: Context<()>) -> (MetadataPrelude, StreamBody) {
        std::future::pending::<()>().await;
        (MetadataPrelude::default(), StreamBody::once(None))
    }

    #[tokio::test]
    async fn the_prelude_then_eight_nul_bytes_then_the_stream_are_posted() {
        let lambda = davidrs::streaming::run(Arc::new(()), count);
        let posted = RuntimeApi::serve(json!({})).await.answer(lambda).await;
        let (prelude, body) = streamed(&posted);
        assert_eq!(
            prelude,
            json!({"statusCode": 201, "headers": {"content-type": "text/plain"}, "cookies": []})
        );
        assert_eq!(body, "1\n2\n3\n");
    }

    #[tokio::test]
    async fn a_handler_that_overruns_its_budget_is_posted_as_a_deadline_error() {
        let lambda = davidrs::streaming::run(Arc::new(()), stall);
        let deadline = epoch_ms_in(Duration::from_millis(300));
        let api = RuntimeApi::serve_with_deadline(json!({}), deadline).await;
        let posted = api.answer(lambda).await;
        assert_eq!(posted.path, error_path());
        let message = json_body(&posted)["errorMessage"].to_string();
        assert!(message.contains("deadline exceeded after"), "{message}");
    }
}

/// An API Gateway HTTP API or Function URL request event.
#[cfg(feature = "http")]
fn http_event(method: &str, path: &str, query: &str, headers: Value) -> Value {
    json!({
        "version": "2.0",
        "routeKey": "$default",
        "rawPath": path,
        "rawQueryString": query,
        "headers": headers,
        "requestContext": {
            "accountId": "123456789012",
            "apiId": "example",
            "domainName": "example.com",
            "domainPrefix": "example",
            "http": {
                "method": method,
                "path": path,
                "protocol": "HTTP/1.1",
                "sourceIp": "203.0.113.9",
                "userAgent": "test"
            },
            "requestId": "api-request-1",
            "routeKey": "$default",
            "stage": "$default",
            "time": "01/Jan/2026:00:00:00 +0000",
            "timeEpoch": 1_767_225_600_000_u64
        },
        "isBase64Encoded": false
    })
}

#[cfg(feature = "http")]
mod http {
    use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request, StatusCode};

    use super::*;

    #[derive(serde::Deserialize)]
    pub(super) struct Lookup {
        name: String,
    }

    pub(super) fn api() -> Api<Public, PlainErrors> {
        Api::new("items", Public, PlainErrors)
    }

    pub(super) fn decode(request: &Request<'_>) -> Result<Lookup, Failure> {
        request.query()
    }

    pub(super) async fn find(
        _: Arc<()>,
        lookup: Lookup,
        _: Context<()>,
    ) -> Result<Json<Value>, Failure> {
        Err(Failure::new(
            StatusCode::NOT_FOUND,
            "ITEM_NOT_FOUND",
            format!("No item named {}", lookup.name),
        ))
    }

    /// Request failures are answers, not invocation errors: the client gets
    /// the rendered status and body, and nothing reaches `/error`.
    #[tokio::test]
    async fn a_request_failure_is_posted_as_the_rendered_response() {
        let lambda = api().run(Arc::new(()), decode, find);
        let event = http_event(
            "GET",
            "/items",
            "name=widget",
            json!({"accept": "application/json"}),
        );
        let posted = RuntimeApi::serve(event).await.answer(lambda).await;
        assert_eq!(posted.path, response_path());
        let response = json_body(&posted);
        assert_eq!(response["statusCode"], 404);
        let body = response["body"].as_str().expect("a text body");
        assert_eq!(
            serde_json::from_str::<Value>(body).expect("a JSON body"),
            json!({"errorCode": "ITEM_NOT_FOUND", "errorMessage": "No item named widget"})
        );
    }
}

#[cfg(feature = "http-stream")]
mod stream {
    use davidrs::http::stream::{
        self as streamed_http, Cors, StreamApi, StreamRequest, StreamResponse,
    };
    use davidrs::http::{Failure, PlainErrors, Public};

    use super::*;

    const ORIGIN: &str = "https://app.example.com";

    pub(super) fn api() -> StreamApi<Public, PlainErrors> {
        StreamApi::new("ticks", Public, PlainErrors).cors(Cors::new(vec![ORIGIN.to_owned()]))
    }

    pub(super) async fn ticks(
        _: Arc<()>,
        _: StreamRequest,
        context: Context<()>,
    ) -> Result<StreamResponse, Failure> {
        Ok(streamed_http::events(
            context.deadline(),
            |events| async move {
                for id in ["1", "2"] {
                    let frame = streamed_http::sse_frame(Some(id), Some("tick"), "{}");
                    if events.send(frame).await.is_err() {
                        return;
                    }
                }
            },
        ))
    }

    #[tokio::test]
    async fn the_head_with_cors_and_vary_then_the_event_stream_are_posted() {
        let lambda = api().run(Arc::new(()), ticks);
        let headers = json!({"origin": ORIGIN, "accept": "text/event-stream"});
        let event = http_event("GET", "/ticks", "", headers);
        let posted = RuntimeApi::serve(event).await.answer(lambda).await;
        let (prelude, body) = streamed(&posted);
        assert_eq!(prelude["statusCode"], 200);
        let headers = &prelude["headers"];
        assert_eq!(headers["content-type"], "text/event-stream; charset=utf-8");
        assert_eq!(headers["vary"], "Origin, Accept");
        assert_eq!(headers["access-control-allow-origin"], ORIGIN);
        assert_eq!(
            body,
            "id: 1\nevent: tick\ndata: {}\n\nid: 2\nevent: tick\ndata: {}\n\n"
        );
    }
}
