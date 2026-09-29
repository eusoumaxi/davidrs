//! X-Ray traces through the public API, read the way the sandbox's X-Ray
//! agent does: one UDP datagram per span, the daemon's JSON header line,
//! then one X-Ray segment document as JSON.
//!
//! A local UDP socket plays the agent. The tests that read or set
//! environment variables, or install the global subscriber, hold [`ENV`].
#![cfg(feature = "otel")]

mod support;

use std::fmt;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use davidrs::telemetry::logs::invocation_span;
use davidrs::telemetry::{agent_endpoint, join_trace, record_status, tracer_provider};
use davidrs::{Deadline, Invocation};
use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
use serde_json::Value;
use tracing::field::Field;
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt as _};

/// Serializes the tests that touch process-wide state.
static ENV: Mutex<()> = Mutex::new(());

/// An `X-Amzn-Trace-Id` value, as Lambda passes it to the function.
const TRACE_HEADER: &str =
    "Root=1-5759e988-bd862e3fe1be46a994272793;Parent=53995c3f42cd8ad8;Sampled=1";

/// The X-Ray daemon UDP header: a JSON object, then a newline, then the
/// segment document the daemon forwards verbatim to `PutTraceSegments`.
const HEADER_LINE: &[u8] = b"{\"format\":\"json\",\"version\":1}\n";

fn agent() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    socket
}

fn address(agent: &UdpSocket) -> String {
    agent.local_addr().expect("address").to_string()
}

/// Receives one datagram and decodes it as the X-Ray daemon does: strip the
/// header line, parse the rest as a segment document.
fn receive(agent: &UdpSocket) -> Value {
    let mut buffer = vec![0_u8; 65_535];
    let read = agent.recv(&mut buffer).expect("a datagram");
    let payload = buffer[..read]
        .strip_prefix(HEADER_LINE)
        .expect("the X-Ray daemon UDP header");
    serde_json::from_slice(payload).expect("a JSON segment document")
}

/// Asserts `value` is a 16-character lowercase hex string, as an X-Ray
/// segment or parent id must be.
fn assert_id(value: &Value, what: &str) {
    let id = value.as_str().expect(what);
    assert_eq!(id.len(), 16, "{what} is 16 hex characters");
    assert!(
        id.chars().all(|c| c.is_ascii_hexdigit()),
        "{what} `{id}` is hex"
    );
}

/// Asserts `value` is an X-Ray trace id `1-<8 hex>-<24 hex>`.
fn assert_trace_id(value: &Value) {
    let trace_id = value.as_str().expect("trace_id");
    let parts: Vec<&str> = trace_id.split('-').collect();
    assert_eq!(parts.len(), 3, "X-Ray trace id has three parts: {trace_id}");
    assert_eq!(parts[0], "1", "the version is 1");
    assert_eq!(parts[1].len(), 8, "the timestamp is 8 hex characters");
    assert_eq!(parts[2].len(), 24, "the random part is 24 hex characters");
    assert!(
        parts[1]
            .chars()
            .chain(parts[2].chars())
            .all(|c| c.is_ascii_hexdigit()),
        "the trace id is hex"
    );
}

#[test]
fn each_span_reaches_the_agent_as_one_xray_segment_datagram() {
    let agent = agent();
    let provider = tracer_provider(&address(&agent), "fallback").expect("provider");
    let mut span = provider.tracer("test").start("unit-of-work");
    span.set_attribute(opentelemetry::KeyValue::new("items", 3));
    span.end();

    let segment = receive(&agent);
    assert_eq!(segment["name"].as_str(), Some("unit-of-work"));
    assert_id(&segment["id"], "segment id");
    assert_trace_id(&segment["trace_id"]);

    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    let stamped = u32::from_str_radix(
        segment["trace_id"]
            .as_str()
            .unwrap()
            .split('-')
            .nth(1)
            .unwrap(),
        16,
    )
    .map(u64::from)
    .expect("hex epoch");
    assert!(
        seconds.abs_diff(stamped) < 60,
        "X-Ray trace ids start with the epoch second"
    );

    assert!(
        segment.get("type").is_none(),
        "a root span has no subsegment type"
    );
    assert!(
        segment.get("parent_id").is_none(),
        "a root span has no parent_id"
    );

    let start = segment["start_time"]
        .as_f64()
        .expect("start_time is a number");
    let end = segment["end_time"].as_f64().expect("end_time is a number");
    assert!(start <= end, "start_time is not after end_time");

    assert_eq!(segment["metadata"]["otel"]["items"].as_i64(), Some(3));
    assert_eq!(segment["service"]["name"].as_str(), Some("fallback"));
    assert!(
        segment.get("origin").is_none(),
        "no Lambda environment: no origin"
    );
}

#[test]
fn the_service_name_is_otel_service_name_then_the_function_name_then_the_fallback() {
    let _env = ENV.lock().unwrap_or_else(PoisonError::into_inner);
    let agent = agent();
    let traced = |name: &str| {
        let provider = tracer_provider(&address(&agent), "fallback").expect("provider");
        provider.tracer("test").start(name.to_owned()).end();
        receive(&agent)
    };

    support::env::remove("OTEL_SERVICE_NAME");
    support::env::remove("AWS_LAMBDA_FUNCTION_NAME");
    let outside = traced("outside-lambda");
    assert_eq!(outside["service"]["name"].as_str(), Some("fallback"));
    assert!(
        outside.get("origin").is_none(),
        "no Lambda environment: no origin"
    );

    support::env::set("AWS_LAMBDA_FUNCTION_NAME", "orders-function");
    let in_lambda = traced("in-lambda");
    assert_eq!(
        in_lambda["service"]["name"].as_str(),
        Some("orders-function")
    );
    assert_eq!(
        in_lambda["origin"].as_str(),
        Some("AWS::Lambda::Function"),
        "a Lambda resource sets the X-Ray origin"
    );

    support::env::set("OTEL_SERVICE_NAME", "orders");
    assert_eq!(traced("named")["service"]["name"].as_str(), Some("orders"));

    support::env::remove("OTEL_SERVICE_NAME");
    support::env::remove("AWS_LAMBDA_FUNCTION_NAME");
}

/// Nothing listens once the agent is gone, and the operating system refuses
/// the next datagram; the traced code carries on regardless.
#[test]
fn a_missing_agent_never_fails_the_traced_code() {
    let endpoint = address(&agent());
    let provider = tracer_provider(&endpoint, "fallback").expect("provider");
    for name in ["first", "second", "third"] {
        provider.tracer("test").start(name).end();
    }
}

#[test]
fn an_endpoint_that_is_not_an_address_is_an_error() {
    let error = tracer_provider("not-an-address", "fallback").expect_err("no socket");
    assert!(error.to_string().contains("X-Ray agent socket"), "{error}");
}

#[test]
fn the_agent_endpoint_is_aws_xray_daemon_address_or_the_default() {
    let _env = ENV.lock().unwrap_or_else(PoisonError::into_inner);
    support::env::set("AWS_XRAY_DAEMON_ADDRESS", "169.254.79.129:2000");
    assert_eq!(agent_endpoint(), "169.254.79.129:2000");
    support::env::remove("AWS_XRAY_DAEMON_ADDRESS");
    assert_eq!(agent_endpoint(), "127.0.0.1:2000");
}

/// Records every event's level and fields as one line of text.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<String>>>);

impl<S: Subscriber> Layer<S> for Recorder {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut line = event.metadata().level().to_string();
        event.record(&mut |field: &Field, value: &dyn fmt::Debug| {
            line.push_str(&format!(" {field}={value:?}"));
        });
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }
}

impl Recorder {
    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// With `RUST_LOG=warn` an invocation span is disabled: there is nothing to
/// join, and nothing to warn about on every invocation.
#[test]
fn a_disabled_span_is_left_alone() {
    let recorder = Recorder::default();
    let subscriber = tracing_subscriber::registry()
        .with(recorder.clone())
        .with(LevelFilter::WARN);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("lambda.invocation");
        assert!(span.is_disabled());
        join_trace(&span, Some(TRACE_HEADER));
    });
    assert_eq!(recorder.lines(), Vec::<String>::new());
}

/// One test, because the global subscriber is installed once per process and
/// the cold-start flag flips once per process.
#[test]
fn init_exports_invocation_spans_as_server_spans_of_the_lambda_trace() {
    let _env = ENV.lock().unwrap_or_else(PoisonError::into_inner);
    support::env::remove("RUST_LOG");
    let agent = agent();

    support::env::set("AWS_XRAY_DAEMON_ADDRESS", "not-an-address");
    assert!(davidrs::telemetry::init("traces-test").is_err());
    support::env::set("AWS_XRAY_DAEMON_ADDRESS", address(&agent));
    let guard = davidrs::telemetry::init("traces-test").expect("installed");
    support::env::remove("AWS_XRAY_DAEMON_ADDRESS");
    assert_eq!(format!("{guard:?}"), "Guard { traces: true }");

    let deadline = Deadline::after(Duration::from_secs(5));
    let first = Invocation::new("request-1", deadline).with_trace_id(Some(TRACE_HEADER.into()));
    let span = invocation_span("orders", &first);
    record_status(&span, 201);
    drop(span);
    let segment = receive(&agent);
    assert_eq!(segment["name"].as_str(), Some("lambda.invocation"));
    assert_id(&segment["id"], "segment id");
    assert_eq!(
        segment["trace_id"].as_str(),
        Some("1-5759e988-bd862e3fe1be46a994272793")
    );
    assert_eq!(segment["type"].as_str(), Some("subsegment"));
    assert_eq!(segment["parent_id"].as_str(), Some("53995c3f42cd8ad8"));
    assert_eq!(
        segment["metadata"]["otel"]["faas.coldstart"].as_bool(),
        Some(true)
    );
    assert_eq!(
        segment["metadata"]["otel"]["http.response.status_code"].as_i64(),
        Some(201)
    );
    assert_eq!(
        segment["metadata"]["otel"]["operation"].as_str(),
        Some("orders")
    );
    assert_eq!(
        segment["metadata"]["otel"]["request_id"].as_str(),
        Some("request-1")
    );
    assert_eq!(segment["service"]["name"].as_str(), Some("traces-test"));

    drop(invocation_span(
        "orders",
        &Invocation::new("request-2", deadline),
    ));
    let second = receive(&agent);
    assert!(second.get("type").is_none(), "a root span is a segment");
    assert!(
        second.get("parent_id").is_none(),
        "a root span has no parent_id"
    );
    assert_eq!(
        second["metadata"]["otel"]["faas.coldstart"].as_bool(),
        Some(false)
    );

    assert!(davidrs::telemetry::init("traces-test").is_err());

    let recorder = Recorder::default();
    let outside = tracing_subscriber::registry().with(recorder.clone());
    tracing::subscriber::with_default(outside, || {
        join_trace(
            &tracing::info_span!("lambda.invocation"),
            Some(TRACE_HEADER),
        );
    });
    let lines = recorder.lines();
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].starts_with("WARN"), "{}", lines[0]);
    assert!(
        lines[0].contains("could not join the X-Ray trace"),
        "{}",
        lines[0]
    );
}
