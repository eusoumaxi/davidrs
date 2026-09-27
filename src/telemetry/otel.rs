//! X-Ray traces, sent to the X-Ray agent Lambda runs in every sandbox.
//!
//! Each finished span leaves as one UDP datagram to the agent at
//! `AWS_XRAY_DAEMON_ADDRESS`, which forwards it to X-Ray outside the
//! invocation. This is the transport the AWS Distro for OpenTelemetry Lambda
//! layers use. A datagram is a JSON header line, the `T1S` prefix for sampled
//! traces, then base64 of an OTLP `ExportTraceServiceRequest`.
//!
//! A local datagram costs microseconds per span. Exporting OTLP over HTTPS
//! would cost a network round trip on the request path, or a batch to flush
//! before the sandbox freezes.

use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry::Context;
use opentelemetry_aws::detector::LambdaResourceDetector;
use opentelemetry_aws::trace::xray_propagator::span_context_from_str;
use opentelemetry_aws::trace::XrayIdGenerator;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, SpanData, SpanExporter};
use opentelemetry_sdk::Resource;
use prost::Message as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::RuntimeError;

/// True until the first invocation span reads it, then false.
///
/// Relaxed ordering is enough: the flag publishes nothing but itself.
static COLD_START: AtomicBool = AtomicBool::new(true);

const UDP_HEADER: &[u8] = b"{\"format\":\"json\",\"version\":1}\nT1S";

/// Exports each batch of spans as one OTLP datagram to the X-Ray agent.
#[derive(Debug)]
struct UdpOtlpExporter {
    socket: std::net::UdpSocket,
    resource: ResourceAttributesWithSchema,
}

impl UdpOtlpExporter {
    fn new(endpoint: &str) -> std::io::Result<Self> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.connect(endpoint)?;
        Ok(Self {
            socket,
            resource: (&Resource::builder_empty().build()).into(),
        })
    }

    fn datagram(&self, batch: Vec<SpanData>) -> Vec<u8> {
        let request = ExportTraceServiceRequest {
            resource_spans: group_spans_by_resource_and_scope(batch, &self.resource),
        };
        let mut out = UDP_HEADER.to_vec();
        out.extend(
            base64::engine::general_purpose::STANDARD
                .encode(request.encode_to_vec())
                .into_bytes(),
        );
        out
    }
}

impl SpanExporter for UdpOtlpExporter {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let result = self
            .socket
            .send(&self.datagram(batch))
            .map(|_| ())
            .map_err(|error| OTelSdkError::InternalFailure(error.to_string()));
        std::future::ready(result)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.into();
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
    let exporter = UdpOtlpExporter::new(endpoint)
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
    if let Some(parent) = xray_trace_header.and_then(span_context_from_str) {
        if let Err(error) = span.set_parent(Context::new().with_remote_span_context(parent)) {
            tracing::warn!(%error, "could not join the X-Ray trace");
        }
    }
}

/// Records an HTTP response status on a span as
/// `http.response.status_code`.
pub fn record_status(span: &tracing::Span, status: u16) {
    span.set_attribute("http.response.status_code", i64::from(status));
}
