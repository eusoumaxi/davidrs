//! Logs, metrics and traces: three independent features.
//!
//! Each is gated on its own, so a function that wants structured logs does not
//! link an OTLP exporter, and a function that emits metrics does not need a
//! tracing pipeline. `init` installs whichever of logs and traces the binary
//! was built with, once, from `main`.
//!
//! | Feature | Gives |
//! | --- | --- |
//! | `logs` | `init`, `logs`: a plain-text subscriber, invocation spans, timed startup |
//! | `metrics` | `Metrics`: CloudWatch Embedded Metric Format documents |
//! | `otel` | `logs` plus X-Ray traces sent to the sandbox's X-Ray agent |

#[cfg(feature = "logs")]
pub mod logs;
#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(feature = "otel")]
pub mod otel;

#[cfg(feature = "logs")]
pub use logs::level_from_env;
#[cfg(feature = "metrics")]
pub use metrics::{Metrics, Unit};
#[cfg(feature = "otel")]
pub use otel::{agent_endpoint, join_trace, record_status, tracer_provider};

/// The telemetry [`init`] installed. Hold it until `main` returns.
///
/// With `otel` it holds the tracer provider. Spans are exported as they end,
/// so there is nothing to flush when the process stops.
#[cfg(feature = "logs")]
#[must_use = "hold the telemetry guard until `main` returns"]
pub struct Guard {
    #[cfg(feature = "otel")]
    _provider: opentelemetry_sdk::trace::SdkTracerProvider,
}

#[cfg(feature = "logs")]
impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard")
            .field("traces", &cfg!(feature = "otel"))
            .finish()
    }
}

/// Installs the process's telemetry: logs, and X-Ray traces when the `otel`
/// feature is on.
///
/// `service` names the traces when neither `OTEL_SERVICE_NAME` nor
/// `AWS_LAMBDA_FUNCTION_NAME` is set, which only happens outside Lambda.
///
/// # Errors
///
/// Returns [`RuntimeError`](crate::RuntimeError) when a global subscriber is
/// already installed or the X-Ray agent socket cannot be opened.
///
/// # Examples
///
/// ```no_run
/// # fn main() -> Result<(), davidrs::RuntimeError> {
/// let _telemetry = davidrs::telemetry::init("orders")?;
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "logs")]
pub fn init(service: &str) -> Result<Guard, crate::RuntimeError> {
    #[cfg(feature = "otel")]
    {
        Ok(Guard {
            _provider: otel::init(service)?,
        })
    }
    #[cfg(not(feature = "otel"))]
    {
        let _ = service;
        logs::init()?;
        Ok(Guard {})
    }
}
