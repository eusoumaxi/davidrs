//! Logs: a plain-text `tracing` subscriber for CloudWatch, the span every
//! pipeline opens around an invocation, and timed startup.
//!
//! Nothing in this crate installs a global subscriber on its own. That is a
//! process-wide decision, and it belongs to the binary's `main`.

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::Layer;

/// The level in `RUST_LOG`: `INFO` when it is unset or not a bare level,
/// and `ERROR` when it is empty, as `tracing` parses it.
///
/// Only a bare level is read (`debug`, `WARN`, `off`, `3`). Directives such as
/// `info,my_crate=debug` need `tracing-subscriber`'s `env-filter`, which pulls
/// in `regex`, a large dependency for a Lambda binary.
#[must_use]
pub fn level_from_env() -> LevelFilter {
    std::env::var("RUST_LOG")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(LevelFilter::INFO)
}

/// Installs a plain-text subscriber at [`level_from_env`] as the process's
/// global default.
///
/// # Errors
///
/// Returns [`RuntimeError`](crate::RuntimeError) when a global subscriber is
/// already installed.
pub fn init() -> Result<(), crate::RuntimeError> {
    tracing_subscriber::registry()
        .with(level_from_env())
        .with(text_layer())
        .try_init()
        .map_err(|error| crate::RuntimeError::other("installing the log subscriber", error))
}

/// Runs one startup step and logs how long it took, then returns its result
/// unchanged.
///
/// The `initialized` event carries `component`, `init_ms` and `success`, never
/// the value or the error: either may hold configuration or a secret.
///
/// # Errors
///
/// Returns the step's own error.
pub async fn timed_init<T, E>(
    component: &'static str,
    work: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let started = std::time::Instant::now();
    let result = work.await;
    let init_ms = started.elapsed().as_millis() as u64;
    let success = result.is_ok();
    tracing::info!(component, init_ms, success, "initialized");
    result
}

/// The `lambda.invocation` span every pipeline opens around one invocation,
/// with the operation and the request id.
///
/// With the `otel` feature it is also the server span of the invocation's
/// X-Ray trace. Applications instrument their own work inside it.
pub fn invocation_span(operation: &'static str, invocation: &crate::Invocation) -> tracing::Span {
    let span = tracing::info_span!("lambda.invocation", operation,
        request_id = %invocation.request_id, "otel.kind" = tracing::field::Empty);
    #[cfg(feature = "otel")]
    super::join_trace(&span, invocation.trace_id.as_deref());
    span
}

/// The formatting layer [`init`] installs: plain text, no ANSI colours and no
/// timestamp, because Lambda stamps every line itself.
///
/// Public so a binary that assembles its own subscriber can reuse it.
pub fn text_layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .without_time()
}
