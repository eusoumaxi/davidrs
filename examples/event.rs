//! An EventBridge rule target that reads a typed `detail`.
//!
//! Each invocation is one event. The handler only validates and logs it: no
//! event is published and nothing is written.

use std::sync::Arc;

use davidrs::event::Event;
use davidrs::{Context, RuntimeError};

/// The `detail` of an `order.created` event.
#[derive(serde::Deserialize)]
struct OrderCreated {
    order_id: String,
}

/// Accepts an event whose detail names an order.
///
/// An error is an invocation error, which the rule's retry policy and
/// dead-letter queue handle.
async fn consume(
    _: Arc<()>,
    event: Event<OrderCreated>,
    _: Context<()>,
) -> Result<(), RuntimeError> {
    if event.detail.order_id.is_empty() {
        return Err(RuntimeError::message("order_id is required"));
    }
    tracing::info!(event_id = %event.id, order_id = %event.detail.order_id, "event accepted");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::telemetry::logs::init()?;
    davidrs::event::run(Arc::new(()), consume).await
}
