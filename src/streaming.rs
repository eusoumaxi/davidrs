//! Response streaming where the producer is owned by the response.
//!
//! The usual way to stream a response detaches the producer:
//!
//! ```ignore
//! let (tx, rx) = mpsc::channel(4);
//! let _ = tokio::spawn(async move { produce(tx).await });
//! Ok(StreamResponse { stream: ReceiverStream::new(rx), .. })
//! ```
//!
//! `let _ =` drops the [`JoinHandle`], which detaches the task. The response
//! can no longer stop it, so it keeps calling upstream services and writing
//! rows after the body is gone, and a panic inside it is invisible: the stream
//! simply ends early and looks complete.
//!
//! [`StreamBody`] owns the handle and a [`CancellationToken`]. Dropping the
//! body cancels the producer, gives it 50 ms to stop on its own, then drops
//! its future. A producer panic surfaces as a final error item instead of a
//! truncated success. Remote side effects are not rolled back.
//!
//! Lambda does not guarantee that a disconnected client stops the invocation,
//! and a client that keeps reading could hold it open, so the producer also
//! carries its own [`Deadline`].
//!
//! # Examples
//!
//! ```
//! use std::time::Duration;
//!
//! use davidrs::streaming::StreamBody;
//! use davidrs::Deadline;
//! use futures_util::StreamExt as _;
//!
//! # #[tokio::main]
//! # async fn main() {
//! let deadline = Deadline::after(Duration::from_secs(5));
//! let body = StreamBody::spawn(4, deadline, |producer| async move {
//!     for line in ["a\n", "b\n"] {
//!         if !producer.send(line).await {
//!             return;
//!         }
//!     }
//! });
//! let chunks: Vec<_> = body.map(|chunk| chunk.expect("chunk")).collect().await;
//! assert_eq!(chunks, ["a\n", "b\n"]);
//! # }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
pub use tokio_util::sync::CancellationToken;

use crate::{Deadline, RuntimeError};

/// What a producer sends: a chunk, or the failure that ended the stream.
pub type Chunk = Result<Bytes, RuntimeError>;

/// The handle a producer writes into.
///
/// [`Producer::send`] returns `false` once the receiver is gone, which is the
/// signal to stop: nobody is reading any more.
#[derive(Debug)]
pub struct Producer {
    sender: mpsc::Sender<Chunk>,
    cancel: CancellationToken,
    deadline: Deadline,
}

impl Producer {
    /// Sends a chunk. Returns `false` when the consumer has gone away.
    pub async fn send(&self, chunk: impl Into<Bytes>) -> bool {
        self.sender.send(Ok(chunk.into())).await.is_ok()
    }

    /// Sends a failure, ending the stream. Returns `false` when the consumer
    /// has gone away.
    pub async fn fail(&self, error: RuntimeError) -> bool {
        self.sender.send(Err(error)).await.is_ok()
    }

    /// Whether the producer should stop: cancelled, out of budget, or nobody
    /// is listening. Check it between units of work.
    pub fn should_stop(&self) -> bool {
        self.cancel.is_cancelled() || self.deadline.is_expired() || self.sender.is_closed()
    }

    /// Resolves when the body is dropped or cancelled, or the producer's
    /// deadline passes.
    ///
    /// Use it in a `select!` so a long upstream call is abandoned promptly.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// The producer's own absolute budget.
    pub fn deadline(&self) -> Deadline {
        self.deadline
    }
}

/// A stream of chunks that owns the task producing them.
///
/// Dropping it stops the producer. Nothing else has to remember to.
#[derive(Debug)]
pub struct StreamBody {
    receiver: mpsc::Receiver<Chunk>,
    producer: Option<JoinHandle<Result<(), RuntimeError>>>,
    cancel: CancellationToken,
    finished: bool,
}

impl StreamBody {
    /// Spawns `produce` and returns a stream that owns it.
    ///
    /// `capacity` is the channel depth, which is the backpressure: a producer
    /// that runs ahead blocks in [`Producer::send`] instead of buffering. A
    /// capacity of zero is read as one.
    ///
    /// When the body is dropped or `deadline` passes, the producer is
    /// cancelled and given 50 ms to release what it holds before its future
    /// is dropped; an expired deadline also ends the stream with
    /// [`RuntimeError::DeadlineExceeded`]. Writes it already made to other
    /// services stay made, so they need idempotency or reconciliation.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime, as [`tokio::spawn`] does.
    pub fn spawn<F, Fut>(capacity: usize, deadline: Deadline, produce: F) -> Self
    where
        F: FnOnce(Producer) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let cancel = CancellationToken::new();
        let stopped = cancel.clone();
        let work = async move {
            let started = std::time::Instant::now();
            let work = produce(Producer {
                sender,
                cancel: stopped.clone(),
                deadline,
            });
            tokio::pin!(work);
            let expired = tokio::select! {
                biased;
                () = stopped.cancelled() => false,
                () = tokio::time::sleep(deadline.remaining()) => true,
                () = &mut work => return if deadline.is_expired() {
                    Err(RuntimeError::DeadlineExceeded { elapsed: started.elapsed() })
                } else {
                    Ok(())
                },
            };
            stopped.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), &mut work).await;
            if expired {
                Err(RuntimeError::DeadlineExceeded {
                    elapsed: started.elapsed(),
                })
            } else {
                Ok(())
            }
        };
        #[cfg(feature = "logs")]
        let work = tracing::Instrument::in_current_span(work);
        let handle = tokio::spawn(work);
        Self {
            receiver,
            producer: Some(handle),
            cancel,
            finished: false,
        }
    }

    /// A stream of one chunk that is already in memory, or of none.
    ///
    /// No producer task is spawned: the chunk is queued before the stream is
    /// returned. Use it for a response that is complete before it starts —
    /// an image, a rendered document, a `HEAD` answer with `None`.
    #[must_use]
    pub fn once(chunk: Option<Bytes>) -> Self {
        let (sender, receiver) = mpsc::channel(1);
        if let Some(chunk) = chunk {
            let _ = sender.try_send(Ok(chunk));
        }
        Self {
            receiver,
            producer: None,
            cancel: CancellationToken::new(),
            finished: false,
        }
    }

    /// Stops the producer and waits for it to finish.
    ///
    /// Prefer this to dropping when the producer must be done before you go
    /// on: `Drop` cannot await.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.producer.take() {
            let _ = handle.await;
        }
    }
}

/// Yields each chunk, then one error item if the producer failed, panicked or
/// ran out of budget.
///
/// A closed channel alone does not end the stream: the producer's task is
/// checked first, so a panic is not mistaken for a complete stream.
impl futures_util::Stream for StreamBody {
    type Item = Chunk;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        match self.receiver.poll_recv(context) {
            Poll::Ready(Some(chunk)) => Poll::Ready(Some(chunk)),
            Poll::Ready(None) => {
                let result = match self.producer.as_mut() {
                    Some(handle) => match Pin::new(handle).poll(context) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(result) => result,
                    },
                    None => return Poll::Ready(None),
                };
                self.finished = true;
                match result {
                    Ok(Ok(())) => Poll::Ready(None),
                    Ok(Err(error)) => Poll::Ready(Some(Err(error))),
                    Err(_) => Poll::Ready(Some(Err(RuntimeError::message(
                        "the stream producer stopped unexpectedly",
                    )))),
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for StreamBody {
    /// Cancels the producer. Its task allows 50 ms of cooperative cleanup,
    /// then drops the producer's future. Use [`StreamBody::shutdown`] to wait
    /// for that.
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Runs a Lambda loop that answers with a stream.
///
/// `handler` builds the response prelude and the [`StreamBody`] for one
/// invocation. The body owns its producer, so returning it hands the runtime
/// something that stops when the response ends.
///
/// # Errors
///
/// Returns a [`RuntimeError`] only when the runtime itself fails.
pub async fn run<App, In, H, F>(app: std::sync::Arc<App>, handler: H) -> Result<(), RuntimeError>
where
    App: Send + Sync + 'static,
    In: serde::de::DeserializeOwned + Send,
    H: Fn(std::sync::Arc<App>, In, crate::Context<()>) -> F + Send + Sync,
    F: std::future::Future<Output = (lambda_runtime::MetadataPrelude, StreamBody)> + Send,
{
    let handler = &handler;
    let service = lambda_runtime::service_fn(move |event: lambda_runtime::LambdaEvent<In>| {
        let app = std::sync::Arc::clone(&app);
        async move {
            let (payload, context) = event.into_parts();
            let invocation = crate::runtime::invocation_from(&context);
            let deadline = invocation
                .deadline
                .with_margin(std::time::Duration::from_millis(100));
            #[cfg(feature = "logs")]
            let span = crate::telemetry::logs::invocation_span("streaming", &invocation);
            let work = handler(app, payload, crate::Context::new(invocation, ()));
            #[cfg(feature = "logs")]
            let work = tracing::Instrument::instrument(work, span);
            let (metadata_prelude, stream) = deadline
                .run(work)
                .await
                .map_err(lambda_runtime::Error::from)?;
            Ok::<_, lambda_runtime::Error>(lambda_runtime::StreamResponse {
                metadata_prelude,
                stream,
            })
        }
    });
    lambda_runtime::Runtime::new(service)
        .run()
        .await
        .map_err(crate::runtime::loop_failure)
}
