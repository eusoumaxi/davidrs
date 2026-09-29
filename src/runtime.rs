//! The native Lambda loop over any typed payload.
//!
//! Prefer the typed adapters — [`http`](crate::http), [`queue`](crate::queue),
//! [`event`](crate::event), [`schedule`](crate::schedule),
//! [`streaming`](crate::streaming). Use [`run`] for a trigger none of them
//! models, or for a function invoked directly with a payload of its own: it
//! gives the same [`Invocation`] metadata and deadline, and nothing else.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Context, Deadline, Invocation, RuntimeError};

/// The invocation error Lambda records when a handler fails: an `errorType`
/// and an `errorMessage`.
///
/// A handler's error becomes one through `Into<Diagnostic>`. `lambda_runtime`
/// converts `String`, `&'static str`, `std::io::Error` and boxed errors, with
/// their Rust type name as `errorType`; [`RuntimeError`] gives its variant,
/// such as `DeadlineExceeded`. Implement `From<YourError> for Diagnostic` to
/// choose the `errorType` yourself, which is what a Step Functions `Retry` or
/// `Catch` matches in `ErrorEquals`:
///
/// ```
/// use davidrs::runtime::Diagnostic;
///
/// /// The failures of a payment step.
/// enum PaymentError {
///     Declined,
/// }
///
/// impl From<PaymentError> for Diagnostic {
///     fn from(error: PaymentError) -> Self {
///         match error {
///             PaymentError::Declined => Diagnostic {
///                 error_type: "PaymentDeclined".to_owned(),
///                 error_message: "the card was declined".to_owned(),
///             },
///         }
///     }
/// }
///
/// let recorded = Diagnostic::from(PaymentError::Declined);
/// assert_eq!(recorded.error_type, "PaymentDeclined");
/// ```
pub use lambda_runtime::Diagnostic;

/// The variant as `errorType`, so a workflow can match a deadline or a limit
/// by name, and the error's `Display` as the message.
impl From<RuntimeError> for Diagnostic {
    fn from(error: RuntimeError) -> Self {
        let error_type = match error {
            RuntimeError::Configuration(_) => "Configuration",
            RuntimeError::DeadlineExceeded { .. } => "DeadlineExceeded",
            RuntimeError::LimitExceeded { .. } => "LimitExceeded",
            RuntimeError::Other { .. } => "Other",
        };
        Self {
            error_type: error_type.to_owned(),
            error_message: error.to_string(),
        }
    }
}

/// Lambda's maximum function timeout.
pub(crate) const MAX_FUNCTION_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The time a handler's budget keeps in hand to post its answer.
const RESERVE: Duration = Duration::from_millis(100);

/// Turns a failure of the Lambda loop itself into a [`RuntimeError`], worded
/// the same for every entry point.
pub(crate) fn loop_failure(error: lambda_runtime::Error) -> RuntimeError {
    RuntimeError::Other {
        context: "lambda runtime".to_owned(),
        source: Some(error),
    }
}

/// Runs a Lambda loop over an arbitrary typed payload.
///
/// `In` is deserialized from the invocation payload and `Out` is serialized
/// as the function's response. The handler runs under the invocation deadline
/// minus 100 ms, kept to post the answer.
///
/// # Errors
///
/// Returns a [`RuntimeError`] when the loop itself fails, such as when the
/// Runtime API is unreachable. A handler error, a payload that does not
/// deserialize and a handler that overruns its budget are each reported to
/// Lambda as an invocation error, and the loop goes on. The handler's error
/// becomes the invocation error through `Into<`[`Diagnostic`]`>`, which
/// decides its `errorType`; an overrun is a `DeadlineExceeded`.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use davidrs::{Context, RuntimeError};
///
/// #[derive(serde::Deserialize)]
/// struct Resize { width: u32 }
///
/// #[derive(serde::Serialize)]
/// struct Resized { pixels: u32 }
///
/// async fn resize(_: Arc<()>, input: Resize, _: Context<()>) -> Result<Resized, RuntimeError> {
///     Ok(Resized { pixels: input.width * input.width })
/// }
///
/// # async fn start() -> Result<(), RuntimeError> {
/// davidrs::runtime::run(Arc::new(()), resize).await
/// # }
/// ```
pub async fn run<App, In, Out, E, H, F>(app: Arc<App>, handler: H) -> Result<(), RuntimeError>
where
    App: Send + Sync + 'static,
    In: DeserializeOwned + Send,
    Out: Serialize,
    E: Into<Diagnostic>,
    H: Fn(Arc<App>, In, Context<()>) -> F + Send + Sync,
    F: Future<Output = Result<Out, E>> + Send,
{
    let handler = &handler;
    let service = lambda_runtime::service_fn(move |event: lambda_runtime::LambdaEvent<In>| {
        let app = Arc::clone(&app);
        async move {
            let (payload, context) = event.into_parts();
            let invocation = invocation_from(&context);
            let deadline = invocation.deadline.with_margin(RESERVE);
            #[cfg(feature = "logs")]
            let span = crate::telemetry::logs::invocation_span("runtime", &invocation);
            let work = handler(app, payload, Context::new(invocation, ()));
            #[cfg(feature = "logs")]
            let work = tracing::Instrument::instrument(work, span);
            deadline
                .run(work)
                .await
                .map_err(Diagnostic::from)?
                .map_err(Into::<Diagnostic>::into)
        }
    });
    lambda_runtime::Runtime::new(service)
        .run()
        .await
        .map_err(loop_failure)
}

/// Builds invocation metadata from the native runtime context.
///
/// The deadline becomes a monotonic instant at once, so a later wall-clock
/// step cannot move it. An invoked ARN that is not an ARN is `None`: the
/// empty string, or a local emulator's placeholder such as the `function-arn`
/// that `cargo lambda watch` sends.
///
/// Lambda sends the deadline as epoch milliseconds. A local emulator may send
/// a relative budget instead (`cargo lambda watch` sends `600000`): read as an
/// epoch, a value no larger than Lambda's 15-minute maximum timeout would fall
/// in the first quarter hour of 1970, so it is read as that long from now. Any
/// larger value is an epoch, and one already past is an expired deadline.
///
/// ```
/// let mut context = lambda_runtime::Context::default();
/// context.request_id = "r-1".to_owned();
/// context.deadline = 600_000;
/// let invocation = davidrs::runtime::invocation_from(&context);
/// assert_eq!(invocation.request_id, "r-1");
/// assert!(!invocation.deadline.is_expired());
/// assert_eq!(invocation.invoked_arn, None);
/// ```
pub fn invocation_from(context: &lambda_runtime::Context) -> Invocation {
    Invocation::new(
        context.request_id.clone(),
        epoch_ms_to_deadline(context.deadline),
    )
    .with_trace_id(context.xray_trace_id.clone())
    .with_invoked_arn(
        Some(context.invoked_function_arn.clone()).filter(|arn| arn.starts_with("arn:")),
    )
    .with_tenant_id(
        context
            .tenant_id
            .clone()
            .filter(|tenant| !tenant.is_empty()),
    )
}

/// Converts the runtime's deadline to a monotonic instant, as
/// [`invocation_from`] describes. The HTTP adapters share it.
pub(crate) fn epoch_ms_to_deadline(deadline_ms: u64) -> Deadline {
    if Duration::from_millis(deadline_ms) <= MAX_FUNCTION_TIMEOUT {
        return Deadline::after(Duration::from_millis(deadline_ms));
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    Deadline::at(Instant::now() + Duration::from_millis(deadline_ms.saturating_sub(now_ms)))
}
