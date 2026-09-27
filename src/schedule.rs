//! Typed scheduled input.
//!
//! EventBridge Scheduler and scheduled rules deliver whatever payload the
//! schedule configures. Deserializing it into a real type is the whole job: a
//! schedule whose payload silently stopped matching the handler is otherwise
//! invisible until the wrong branch runs.

use std::future::Future;
use std::sync::Arc;

use crate::{Context, RuntimeError};

/// Runs a scheduled handler over a typed payload.
///
/// Use `()` when the schedule has no payload, or [`serde_json::Value`] to
/// accept anything.
///
/// # Errors
///
/// Returns a [`RuntimeError`] only when the loop itself fails. A handler
/// error, or a payload that does not deserialize into `T`, is reported to
/// Lambda as an invocation error, so the schedule's retry policy and
/// dead-letter queue see it.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use davidrs::{Context, RuntimeError};
///
/// #[derive(serde::Deserialize)]
/// struct Purge { older_than_days: u32 }
///
/// async fn purge(_: Arc<()>, input: Purge, _: Context<()>) -> Result<(), RuntimeError> {
///     println!("purging items older than {} days", input.older_than_days);
///     Ok(())
/// }
///
/// # async fn start() -> Result<(), RuntimeError> {
/// davidrs::schedule::run(Arc::new(()), purge).await
/// # }
/// ```
pub async fn run<App, T, E, H, F>(app: Arc<App>, handler: H) -> Result<(), RuntimeError>
where
    App: Send + Sync + 'static,
    T: serde::de::DeserializeOwned + Send,
    E: std::fmt::Display,
    H: Fn(Arc<App>, T, Context<()>) -> F + Send + Sync,
    F: Future<Output = Result<(), E>> + Send,
{
    let handler = &handler;
    crate::runtime::run(app, move |app, payload: T, context| async move {
        handler(app, payload, context)
            .await
            .map(|()| serde_json::Value::Null)
    })
    .await
}
