//! X-Ray traces, sent to the X-Ray agent Lambda runs in every sandbox.
//!
//! Each finished span leaves as one UDP datagram to the agent at
//! `AWS_XRAY_DAEMON_ADDRESS`, which forwards it to X-Ray outside the
//! invocation. A datagram is the X-Ray daemon's UDP header
//! (`{"format":"json","version":1}`) on one line, then one [X-Ray segment
//! document](https://docs.aws.amazon.com/xray/latest/devguide/xray-api-segmentdocuments.html)
//! as JSON. The daemon forwards that body verbatim to `PutTraceSegments`, so
//! it must be valid X-Ray segment JSON, not OTLP.
//!
//! Local UDP avoids an HTTPS round trip or a batch to flush before the
//! sandbox freezes. Delivery is best effort: a successful send does not
//! confirm that X-Ray received the trace.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use opentelemetry::Context;
use opentelemetry::trace::{SpanId, Status, TraceContextExt as _, TracerProvider as _};
use opentelemetry::{Array, Key, KeyValue, TraceId, Value};
use opentelemetry_aws::detector::LambdaResourceDetector;
use opentelemetry_aws::trace::XrayIdGenerator;
use opentelemetry_aws::trace::xray_propagator::span_context_from_str;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, SpanData, SpanExporter};
use serde_json::Value as JsonValue;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::RuntimeError;

/// True until the first invocation span reads it, then false.
///
/// Relaxed ordering is enough: the flag publishes nothing but itself.
static COLD_START: AtomicBool = AtomicBool::new(true);

/// The X-Ray daemon's UDP framing: a JSON header line, then the segment
/// document on the next line. The daemon validates the header
/// (`format == "json"` and `version == 1`) and forwards what follows the
/// newline verbatim to `PutTraceSegments`.
const UDP_HEADER: &[u8] = b"{\"format\":\"json\",\"version\":1}\n";

/// Exports each finished span to the X-Ray agent as one segment document.
#[derive(Debug)]
struct UdpXrayExporter {
    socket: std::net::UdpSocket,
    resource: Resource,
}

impl UdpXrayExporter {
    fn new(endpoint: &str) -> std::io::Result<Self> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.connect(endpoint)?;
        Ok(Self {
            socket,
            resource: Resource::builder_empty().build(),
        })
    }

    /// One X-Ray segment document for `span`, ready to serialise after
    /// [`UDP_HEADER`].
    ///
    /// A span with a parent becomes a subsegment (`type: "subsegment"` with
    /// `parent_id`); a root span becomes a segment. `OTel` trace and span ids
    /// are rewritten as X-Ray ids: the trace id gains the `1-` version and the
    /// timestamp split, and the segment id is the 16-hex span id.
    fn segment(&self, span: &SpanData) -> JsonValue {
        let mut document = serde_json::Map::new();
        document.insert(
            "name".to_string(),
            JsonValue::String(span.name.as_ref().to_owned()),
        );
        document.insert(
            "id".to_string(),
            JsonValue::String(span.span_context.span_id().to_string()),
        );
        document.insert(
            "trace_id".to_string(),
            JsonValue::String(xray_trace_id(span.span_context.trace_id())),
        );
        document.insert("start_time".to_string(), json_time(span.start_time));
        document.insert("end_time".to_string(), json_time(span.end_time));
        if span.parent_span_id != SpanId::INVALID {
            document.insert(
                "parent_id".to_string(),
                JsonValue::String(span.parent_span_id.to_string()),
            );
            document.insert(
                "type".to_string(),
                JsonValue::String("subsegment".to_owned()),
            );
        }
        if let Some(name) = self.service_name() {
            let mut service = serde_json::Map::new();
            service.insert("name".to_string(), JsonValue::String(name));
            document.insert("service".to_string(), JsonValue::Object(service));
        }
        if self.is_lambda() {
            document.insert(
                "origin".to_string(),
                JsonValue::String("AWS::Lambda::Function".to_owned()),
            );
        }
        if let Some(metadata) = segment_metadata(span) {
            document.insert("metadata".to_string(), metadata);
        }
        if matches!(span.status, Status::Error { .. }) {
            document.insert("error".to_string(), JsonValue::Bool(true));
        }
        JsonValue::Object(document)
    }

    /// The `service.name` resource attribute, when one is set.
    fn service_name(&self) -> Option<String> {
        self.resource
            .get(&Key::from("service.name"))
            .and_then(|value| match value {
                Value::String(string) => Some(string.as_str().to_owned()),
                _ => None,
            })
    }

    /// Whether the resource carries Lambda attributes, i.e. the function is
    /// running inside the Lambda runtime.
    fn is_lambda(&self) -> bool {
        self.resource.get(&Key::from("faas.name")).is_some()
    }
}

impl SpanExporter for UdpXrayExporter {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let result = (|| {
            for span in &batch {
                let body = serde_json::to_vec(&self.segment(span))
                    .map_err(|error| OTelSdkError::InternalFailure(error.to_string()))?;
                let mut datagram = UDP_HEADER.to_vec();
                datagram.extend_from_slice(&body);
                self.socket
                    .send(&datagram)
                    .map_err(|error| OTelSdkError::InternalFailure(error.to_string()))?;
            }
            Ok(())
        })();
        std::future::ready(result)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.clone();
    }
}

/// Converts an `OTel` trace id to the X-Ray trace id form
/// `1-<8 hex timestamp>-<24 hex>`.
fn xray_trace_id(id: TraceId) -> String {
    let hex = id.to_string();
    let (epoch, rest) = hex.split_at(8);
    format!("1-{epoch}-{rest}")
}

/// A `SystemTime` as floating-point seconds since the Unix epoch, the form
/// X-Ray takes for `start_time` and `end_time`.
fn json_time(time: SystemTime) -> JsonValue {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    JsonValue::from(seconds)
}

/// Builds the `metadata` field for `span`: every attribute under one
/// namespace, the way the OpenTelemetry Collector's `awsxrayexporter` places
/// non-indexed attributes by default.
fn segment_metadata(span: &SpanData) -> Option<JsonValue> {
    if span.attributes.is_empty() {
        return None;
    }
    let mut attributes = serde_json::Map::new();
    for KeyValue { key, value, .. } in &span.attributes {
        attributes.insert(key.to_string(), json_value(value));
    }
    let mut namespaces = serde_json::Map::new();
    namespaces.insert("otel".to_string(), JsonValue::Object(attributes));
    Some(JsonValue::Object(namespaces))
}

/// Converts an `OTel` attribute value to JSON for X-Ray metadata.
///
/// Kinds this code does not know about (the enum is non-exhaustive) fall
/// back to their string form, so no attribute is dropped.
fn json_value(value: &Value) -> JsonValue {
    match value {
        Value::Bool(boolean) => JsonValue::Bool(*boolean),
        Value::I64(number) => JsonValue::from(*number),
        Value::F64(number) => JsonValue::from(*number),
        Value::String(string) => JsonValue::String(string.as_str().to_owned()),
        Value::Array(array) => JsonValue::Array(json_array(array)),
        _ => JsonValue::String(value.as_str().into_owned()),
    }
}

/// Converts an `OTel` array attribute to a JSON array.
fn json_array(array: &Array) -> Vec<JsonValue> {
    match array {
        Array::Bool(items) => items.iter().copied().map(JsonValue::Bool).collect(),
        Array::I64(items) => items.iter().copied().map(JsonValue::from).collect(),
        Array::F64(items) => items.iter().copied().map(JsonValue::from).collect(),
        Array::String(items) => items
            .iter()
            .map(|string| JsonValue::String(string.as_str().to_owned()))
            .collect(),
        _ => Vec::new(),
    }
}

/// A tracer provider that exports every span to the X-Ray agent at `endpoint`
/// (`host:port`) as it ends.
///
/// The service name is `OTEL_SERVICE_NAME`, else `AWS_LAMBDA_FUNCTION_NAME`,
/// else `service_fallback`. Inside Lambda the resource also carries the
/// function's `cloud.*` and `faas.*` attributes (region, name, version,
/// memory). Every span is sampled, and trace ids start with a timestamp, as
/// X-Ray requires. Export is synchronous, with no background thread and
/// nothing to flush.
///
/// # Errors
///
/// Returns [`RuntimeError`] when the socket cannot be opened, for example
/// because `endpoint` is not an address.
pub fn tracer_provider(
    endpoint: &str,
    service_fallback: &str,
) -> Result<SdkTracerProvider, RuntimeError> {
    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .or_else(|_| std::env::var("AWS_LAMBDA_FUNCTION_NAME"))
        .unwrap_or_else(|_| service_fallback.to_owned());
    let resource = Resource::builder()
        .with_detector(Box::new(LambdaResourceDetector))
        .with_service_name(service_name)
        .build();
    let exporter = UdpXrayExporter::new(endpoint)
        .map_err(|error| RuntimeError::other("opening the X-Ray agent socket", error))?;
    Ok(SdkTracerProvider::builder()
        .with_simple_exporter(exporter)
        .with_id_generator(XrayIdGenerator::default())
        .with_sampler(Sampler::AlwaysOn)
        .with_resource(resource)
        .build())
}

/// The X-Ray agent address Lambda sets in `AWS_XRAY_DAEMON_ADDRESS`, or
/// `127.0.0.1:2000`, the agent's default, when it is unset.
#[must_use]
pub fn agent_endpoint() -> String {
    std::env::var("AWS_XRAY_DAEMON_ADDRESS").unwrap_or_else(|_| "127.0.0.1:2000".to_owned())
}

/// Installs logs and X-Ray traces as the global subscriber, and returns the
/// tracer provider. [`telemetry::init`](super::init) calls it.
///
/// Spans are not started when they are first entered, so a pipeline can
/// still give the invocation span its X-Ray parent ([`join_trace`]) after
/// creating it.
///
/// # Errors
///
/// Returns [`RuntimeError`] when the agent socket cannot be opened or a
/// global subscriber is already installed.
pub fn init(service_fallback: &str) -> Result<SdkTracerProvider, RuntimeError> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let provider = tracer_provider(&agent_endpoint(), service_fallback)?;
    tracing_subscriber::registry()
        .with(super::logs::level_from_env())
        .with(super::logs::runtime_span_filter())
        .with(super::logs::text_layer())
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("davidrs"))
                .with_context_activation(false),
        )
        .try_init()
        .map_err(|error| {
            crate::RuntimeError::other("installing the telemetry subscriber", error)
        })?;
    Ok(provider)
}

/// Makes `span` the server span of the invocation's X-Ray trace.
///
/// The span gets `otel.kind = SERVER`, `faas.coldstart` (true for the first
/// span this process joins, then false) and, when `xray_trace_header` is a
/// valid `X-Amzn-Trace-Id` value, that trace as its parent.
///
/// A disabled span (its level is filtered out) is left alone. Under a
/// subscriber without the OpenTelemetry layer, the parent cannot be set and a
/// warning is logged instead.
pub fn join_trace(span: &tracing::Span, xray_trace_header: Option<&str>) {
    if span.is_disabled() {
        return;
    }
    span.record("otel.kind", "SERVER");
    span.set_attribute("faas.coldstart", COLD_START.swap(false, Ordering::Relaxed));
    if let Some(parent) = xray_trace_header.and_then(span_context_from_str)
        && let Err(error) = span.set_parent(Context::new().with_remote_span_context(parent))
    {
        tracing::warn!(%error, "could not join the X-Ray trace");
    }
}

/// Records an HTTP response status on a span as
/// `http.response.status_code`.
pub fn record_status(span: &tracing::Span, status: u16) {
    span.set_attribute("http.response.status_code", i64::from(status));
}
