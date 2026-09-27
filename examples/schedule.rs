//! A scheduled function over a typed payload.
//!
//! The schedule's payload is `{"older_than_days": 30}`. A payload that no
//! longer matches [`Purge`] fails the invocation instead of running with
//! defaults. The handler only logs: nothing is deleted.

use std::sync::Arc;

use davidrs::{Context, RuntimeError};

/// The payload the schedule is configured with.
#[derive(serde::Deserialize)]
struct Purge {
    older_than_days: u32,
}

/// Accepts a purge request with a positive age.
async fn purge(_: Arc<()>, input: Purge, context: Context<()>) -> Result<(), RuntimeError> {
    if input.older_than_days == 0 {
        return Err(RuntimeError::message("older_than_days must be positive"));
    }
    tracing::info!(
        request_id = %context.invocation().request_id,
        older_than_days = input.older_than_days,
        "purge accepted"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::telemetry::logs::init()?;
    davidrs::schedule::run(Arc::new(()), purge).await
}
