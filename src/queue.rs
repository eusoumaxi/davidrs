//! SQS consumers with per-record failure reporting.
//!
//! Returning `Err` from an SQS function retries the **whole batch**, including
//! the records that succeeded. This adapter reports failures per record
//! instead, so one poisoned message cannot force nine good ones to be
//! processed again. A record that overruns the deadline, and every record
//! after it, is reported as failed rather than silently dropped.
//!
//! The partial response only works when the event source mapping has
//! `ReportBatchItemFailures` enabled; without it, Lambda ignores the response
//! and deletes the whole batch.

use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{Context, Invocation, RuntimeError};

/// What should happen to one message after the handler ran.
///
/// A handler returns `Retry` for a message it chose not to process yet, and
/// `Err` for one that failed; both are redelivered, and only `Err` is logged
/// (with the `logs` feature).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Disposition {
    /// Processed. SQS deletes the message.
    Delete,
    /// Not processed. SQS redelivers it after the visibility timeout.
    Retry,
}

/// One delivered message.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Delivery {
    /// The SQS message id, used to report a per-record failure.
    pub message_id: String,
    /// The handle needed to change visibility or delete the message.
    pub receipt_handle: String,
    /// The raw message body.
    pub body: String,
    /// SQS-supplied attributes, including `ApproximateReceiveCount`.
    #[serde(default)]
    pub attributes: std::collections::HashMap<String, String>,
    /// The ARN of the queue that delivered it.
    ///
    /// SQS spells this `eventSourceARN`, which `camelCase` does not produce.
    #[serde(default, rename = "eventSourceARN")]
    pub event_source_arn: Option<String>,
}

impl Delivery {
    /// How many times SQS has delivered this message, from
    /// `ApproximateReceiveCount`.
    ///
    /// Returns `1` when the attribute is absent or unparseable: an unknown
    /// count reads as a first attempt, never as an exhausted one.
    pub fn receive_count(&self) -> u32 {
        self.attributes
            .get("ApproximateReceiveCount")
            .and_then(|value| value.parse().ok())
            .unwrap_or(1)
    }

    /// Deserializes the body as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`] naming the message when the body is not the
    /// expected shape.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, RuntimeError> {
        serde_json::from_str(&self.body).map_err(|error| {
            RuntimeError::other(format!("decoding message {}", self.message_id), error)
        })
    }
}

/// The batch Lambda delivers, in the native SQS event shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct Batch {
    /// The messages in this invocation.
    #[serde(rename = "Records")]
    pub records: Vec<Delivery>,
}

/// The partial-batch response Lambda expects: `{"batchItemFailures": [...]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct BatchResponse {
    /// Messages that must be redelivered.
    pub batch_item_failures: Vec<ItemFailure>,
}

/// One message to redeliver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ItemFailure {
    /// The failed message's id.
    pub item_identifier: String,
}

impl BatchResponse {
    /// Records that a message must be redelivered.
    pub fn fail(&mut self, message_id: impl Into<String>) {
        self.batch_item_failures.push(ItemFailure {
            item_identifier: message_id.into(),
        });
    }
}

/// Runs an SQS consumer on the Lambda runtime, one record at a time, in
/// order.
///
/// Each invocation goes through [`process`]. A handler error is logged with
/// the message id only, never the error text, which the application
/// classifies itself.
///
/// # Errors
///
/// Returns a [`RuntimeError`] only when the runtime itself fails.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use davidrs::queue::{Delivery, Disposition};
/// use davidrs::{Context, RuntimeError};
///
/// async fn consume(_app: Arc<()>, delivery: Delivery, _context: Context<()>) -> Result<Disposition, RuntimeError> {
///     let order: serde_json::Value = delivery.json()?;
///     Ok(if order["ready"] == true { Disposition::Delete } else { Disposition::Retry })
/// }
///
/// # async fn start() -> Result<(), RuntimeError> {
/// davidrs::queue::run(Arc::new(()), consume).await
/// # }
/// ```
pub async fn run<App, E, H, F>(app: Arc<App>, handler: H) -> Result<(), RuntimeError>
where
    App: Send + Sync + 'static,
    E: std::fmt::Display,
    H: Fn(Arc<App>, Delivery, Context<()>) -> F + Send + Sync,
    F: Future<Output = Result<Disposition, E>> + Send,
{
    let handler = &handler;
    let service = lambda_runtime::service_fn(move |event: lambda_runtime::LambdaEvent<Batch>| {
        let app = Arc::clone(&app);
        async move {
            let (batch, context) = event.into_parts();
            let invocation = crate::runtime::invocation_from(&context);
            let work = process(app, batch, &invocation, handler);
            #[cfg(feature = "logs")]
            let work = tracing::Instrument::instrument(
                work,
                crate::telemetry::logs::invocation_span("queue", &invocation),
            );
            Ok::<_, lambda_runtime::Error>(work.await)
        }
    });
    lambda_runtime::Runtime::new(service)
        .run()
        .await
        .map_err(crate::runtime::loop_failure)
}

/// Processes one batch and returns the records SQS must redeliver.
///
/// Records run in order under the invocation deadline minus a 100 ms reserve
/// for the response. A record whose handler returns `Err` or
/// [`Disposition::Retry`] is reported; on a FIFO queue (an ARN ending in
/// `.fifo`) that also stops the batch and reports every later record, so
/// messages of a group are never processed out of order. When the budget runs
/// out, the record in progress and every record after it are reported, so the
/// function answers before its deadline and nothing is lost.
///
/// [`run`] calls it for every invocation; call it directly to test a handler
/// without the runtime.
pub async fn process<App, E, H, F>(
    app: Arc<App>,
    batch: Batch,
    invocation: &Invocation,
    handler: &H,
) -> BatchResponse
where
    E: std::fmt::Display,
    H: Fn(Arc<App>, Delivery, Context<()>) -> F,
    F: Future<Output = Result<Disposition, E>>,
{
    let mut response = BatchResponse::default();
    let deadline = invocation
        .deadline
        .with_margin(std::time::Duration::from_millis(100));
    let mut records = batch.records.into_iter();
    for delivery in records.by_ref() {
        if deadline.expired() {
            response.fail(delivery.message_id);
            break;
        }
        let message_id = delivery.message_id.clone();
        let fifo = delivery
            .event_source_arn
            .as_deref()
            .is_some_and(|arn| arn.ends_with(".fifo"));
        let context = Context::new(invocation.clone(), ());
        match deadline
            .run(handler(Arc::clone(&app), delivery, context))
            .await
        {
            Ok(Ok(Disposition::Delete)) => {}
            Ok(Ok(Disposition::Retry)) => {
                response.fail(message_id);
                if fifo {
                    break;
                }
            }
            Ok(Err(_error)) => {
                #[cfg(feature = "logs")]
                tracing::error!(message_id = %message_id, "message failed");
                response.fail(message_id);
                if fifo {
                    break;
                }
            }
            Err(_) => {
                response.fail(message_id);
                break;
            }
        }
    }
    for skipped in records {
        response.fail(skipped.message_id);
    }
    response
}

#[cfg(feature = "queue-visibility")]
mod visibility {
    use std::time::Duration;

    use aws_sdk_sqs::Client;

    use super::Delivery;
    use crate::RuntimeError;

    /// The longest visibility timeout SQS accepts: 12 hours.
    pub const MAX_VISIBILITY_TIMEOUT: Duration = Duration::from_secs(12 * 60 * 60);

    /// The right to change one message's visibility timeout.
    ///
    /// It pairs a queue URL with a receipt handle, the combination SQS needs.
    /// A message *id* is not a receipt handle, and passing one produces a
    /// confusing service error, so this type is built only from a delivery.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Visibility {
        queue_url: String,
        receipt_handle: String,
    }

    impl Visibility {
        /// Builds a handle for a delivery from a known queue.
        #[must_use]
        pub fn new(queue_url: impl Into<String>, delivery: &Delivery) -> Self {
            Self {
                queue_url: queue_url.into(),
                receipt_handle: delivery.receipt_handle.clone(),
            }
        }

        /// Hides this message for `delay`, counted from now, after which SQS
        /// delivers it again.
        ///
        /// Use it to delay a retry: an upstream that asked to be called again
        /// in four seconds should not wait for the queue's thirty. Set the
        /// delay, then return [`Disposition::Retry`](super::Disposition::Retry).
        /// SQS counts whole seconds, so a fraction of a second is rounded up.
        ///
        /// # Errors
        ///
        /// Returns [`RuntimeError::Configuration`], without calling SQS, for a
        /// delay above [`MAX_VISIBILITY_TIMEOUT`], and another
        /// [`RuntimeError`] when the call fails. A receipt handle expires with
        /// the current visibility window, so a late call fails.
        pub async fn set(&self, client: &Client, delay: Duration) -> Result<(), RuntimeError> {
            if delay > MAX_VISIBILITY_TIMEOUT {
                return Err(RuntimeError::Configuration(format!(
                    "a visibility timeout of {delay:?} is above the 12-hour maximum"
                )));
            }
            let seconds = delay.as_secs() + u64::from(delay.subsec_nanos() > 0);
            client
                .change_message_visibility()
                .queue_url(&self.queue_url)
                .receipt_handle(&self.receipt_handle)
                .visibility_timeout(seconds as i32)
                .send()
                .await
                .map(|_| ())
                .map_err(|error| RuntimeError::other("changing message visibility", error))
        }
    }
}

#[cfg(feature = "queue-visibility")]
pub use visibility::{Visibility, MAX_VISIBILITY_TIMEOUT};
