//! X-Ray traces through the public API, read the way the sandbox's X-Ray
//! agent reads them: one UDP datagram per span, a JSON header line, `T1S`,
//! then base64 of an OTLP `ExportTraceServiceRequest`.
//!
//! A local UDP socket plays the agent. The tests that read or set environment
//! variables, or install the global subscriber, hold [`ENV`].
#![cfg(feature = "otel")]

use std::fmt;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use davidrs::telemetry::logs::invocation_span;
use davidrs::telemetry::{agent_endpoint, join_trace, record_status, tracer_provider};
use davidrs::{Deadline, Invocation};
use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::trace::v1::span::SpanKind;
use prost::Message as _;
use tracing::field::Field;
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::Layer;

/// Serializes the tests that touch process-wide state.
static ENV: Mutex<()> = Mutex::new(());

/// An `X-Amzn-Trace-Id` value, as Lambda passes it to the function.
const TRACE_HEADER: &str =
    "Root=1-5759e988-bd862e3fe1be46a994272793;Parent=53995c3f42cd8ad8;Sampled=1";

const HEADER_LINE: &[u8] = b"{\"format\":\"json\",\"version\":1}\nT1S";

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

/// Receives one datagram and decodes it as the agent does.
fn receive(agent: &UdpSocket) -> ExportTraceServiceRequest {
    let mut buffer = vec![0_u8; 65_535];
    let read = agent.recv(&mut buffer).expect("a datagram");
    let payload = buffer[..read]
        .strip_prefix(HEADER_LINE)
        .expect("the header line and T1S");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .expect("base64");
    ExportTraceServiceRequest::decode(bytes.as_slice()).expect("OTLP protobuf")
}

fn attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a Value> {
    attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref()?.value.as_ref())
}

fn string(value: &str) -> Value {
    Value::StringValue(value.to_owned())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn service_name(request: &ExportTraceServiceRequest) -> Option<&Value> {
    let resource = request.resource_spans[0].resource.as_ref()?;
    attribute(&resource.attributes, "service.name")
}

#[test]
fn each_span_reaches_the_agent_as_one_otlp_datagram() {
    let agent = agent();
    let provider = tracer_provider(&address(&agent), "fallback").expect("provider");
    let mut span = provider.tracer("test").start("unit-of-work");
    span.set_attribute(opentelemetry::KeyValue::new("items", 3));
    span.end();

    let request = receive(&agent);
    let span = &request.resource_spans[0].scope_spans[0].spans[0];
    assert_eq!(span.name, "unit-of-work");
    assert_eq!(
        attribute(&span.attributes, "items"),
        Some(&Value::IntValue(3))
    );
    assert_eq!(span.trace_id.len(), 16);
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    let stamped = u64::from(u32::from_be_bytes(
        span.trace_id[..4].try_into().expect("four bytes"),
    ));
    assert!(
        seconds.abs_diff(stamped) < 60,
        "X-Ray trace ids start with the epoch second"
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

    std::env::remove_var("OTEL_SERVICE_NAME");
    std::env::remove_var("AWS_LAMBDA_FUNCTION_NAME");
    assert_eq!(
        service_name(&traced("outside-lambda")),
        Some(&string("fallback"))
    );

    std::env::set_var("AWS_LAMBDA_FUNCTION_NAME", "orders-function");
    let in_lambda = traced("in-lambda");
    assert_eq!(service_name(&in_lambda), Some(&string("orders-function")));
    let resource = in_lambda.resource_spans[0]
        .resource
        .as_ref()
        .expect("resource");
    assert_eq!(
        attribute(&resource.attributes, "faas.name"),
        Some(&string("orders-function"))
    );

    std::env::set_var("OTEL_SERVICE_NAME", "orders");
    assert_eq!(service_name(&traced("named")), Some(&string("orders")));

    std::env::remove_var("OTEL_SERVICE_NAME");
    std::env::remove_var("AWS_LAMBDA_FUNCTION_NAME");
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
    std::env::set_var("AWS_XRAY_DAEMON_ADDRESS", "169.254.79.129:2000");
    assert_eq!(agent_endpoint(), "169.254.79.129:2000");
    std::env::remove_var("AWS_XRAY_DAEMON_ADDRESS");
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
    std::env::remove_var("RUST_LOG");
    let agent = agent();

    std::env::set_var("AWS_XRAY_DAEMON_ADDRESS", "not-an-address");
    assert!(davidrs::telemetry::init("traces-test").is_err());
    std::env::set_var("AWS_XRAY_DAEMON_ADDRESS", address(&agent));
    let guard = davidrs::telemetry::init("traces-test").expect("installed");
    std::env::remove_var("AWS_XRAY_DAEMON_ADDRESS");
    assert_eq!(format!("{guard:?}"), "Guard { traces: true }");

    let deadline = Deadline::in_from_now(Duration::from_secs(5));
    let first = Invocation::new("request-1", deadline).with_trace_id(Some(TRACE_HEADER.into()));
    let span = invocation_span("orders", &first);
    record_status(&span, 201);
    drop(span);
    let request = receive(&agent);
    let exported = &request.resource_spans[0].scope_spans[0].spans[0];
    assert_eq!(exported.name, "lambda.invocation");
    assert_eq!(exported.kind, SpanKind::Server as i32);
    assert_eq!(hex(&exported.trace_id), "5759e988bd862e3fe1be46a994272793");
    assert_eq!(hex(&exported.parent_span_id), "53995c3f42cd8ad8");
    assert_eq!(
        attribute(&exported.attributes, "faas.coldstart"),
        Some(&Value::BoolValue(true))
    );
    assert_eq!(
        attribute(&exported.attributes, "http.response.status_code"),
        Some(&Value::IntValue(201))
    );
    assert_eq!(
        attribute(&exported.attributes, "operation"),
        Some(&string("orders"))
    );
    assert_eq!(
        attribute(&exported.attributes, "request_id"),
        Some(&string("request-1"))
    );
    assert_eq!(service_name(&request), Some(&string("traces-test")));

    drop(invocation_span(
        "orders",
        &Invocation::new("request-2", deadline),
    ));
    let request = receive(&agent);
    let exported = &request.resource_spans[0].scope_spans[0].spans[0];
    assert_eq!(
        attribute(&exported.attributes, "faas.coldstart"),
        Some(&Value::BoolValue(false))
    );
    assert!(exported.parent_span_id.is_empty());

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
