//! Typed EventBridge input.
//!
//! This is the receiving side. Publishing lives behind the `eventbridge` feature,
//! so a consumer does not link an SDK client it never calls.

use std::future::Future;
use std::sync::Arc;

use serde::Deserialize;

use crate::{Context, RuntimeError};

/// One EventBridge event with a typed `detail`.
///
/// Only the envelope fields a consumer acts on are read; the rest (`account`,
/// `region`, `resources`, …) are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub struct Event<T> {
    /// The event id EventBridge assigned.
    pub id: String,
    /// The publisher's source, e.g. `shop.orders`.
    pub source: String,
    /// The event name, e.g. `order.created`.
    pub detail_type: String,
    /// When EventBridge received it, as an RFC 3339 string.
    pub time: Option<String>,
    /// The typed payload.
    pub detail: T,
}

/// Runs an EventBridge consumer: one [`Event<T>`] per invocation.
///
/// # Errors
///
/// Returns a [`RuntimeError`] only when the loop itself fails. A handler
/// error, or an event whose `detail` does not deserialize into `T`, is
/// reported to Lambda as an invocation error. EventBridge invokes the
/// function asynchronously, so Lambda's asynchronous retries and the
/// function's on-failure destination or dead-letter queue act on it.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use davidrs::event::Event;
/// use davidrs::{Context, RuntimeError};
///
/// #[derive(serde::Deserialize)]
/// struct OrderCreated { order_id: String }
///
/// async fn on_order(_: Arc<()>, event: Event<OrderCreated>, _: Context<()>) -> Result<(), RuntimeError> {
///     println!("{} created", event.detail.order_id);
///     Ok(())
/// }
///
/// # async fn start() -> Result<(), RuntimeError> {
/// davidrs::event::run(Arc::new(()), on_order).await
/// # }
/// ```
pub async fn run<App, T, E, H, F>(app: Arc<App>, handler: H) -> Result<(), RuntimeError>
where
    App: Send + Sync + 'static,
    T: serde::de::DeserializeOwned + Send,
    E: Into<crate::runtime::Diagnostic>,
    H: Fn(Arc<App>, Event<T>, Context<()>) -> F + Send + Sync,
    F: Future<Output = Result<(), E>> + Send,
{
    let handler = &handler;
    crate::runtime::run(app, move |app, event: Event<T>, context| async move {
        handler(app, event, context)
            .await
            .map(|()| serde_json::Value::Null)
    })
    .await
}
