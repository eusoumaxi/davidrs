//! An SQS consumer: a typed message body and an explicit delete or retry.
//!
//! Needs no AWS account: run it with `cargo lambda watch --example queue
//! --features queue,logs` and send it an SQS event with `cargo lambda invoke`.

use std::sync::Arc;

use davidrs::queue::{Delivery, Disposition};
use davidrs::{Context, RuntimeError};

/// The body of every message on the queue.
#[derive(serde::Deserialize)]
struct OrderPlaced {
    order_id: String,
    paid: bool,
}

/// Deletes paid orders, asks for unpaid ones again, and fails a message
/// without an order id.
async fn consume(
    _app: Arc<()>,
    delivery: Delivery,
    _context: Context<()>,
) -> Result<Disposition, RuntimeError> {
    let order: OrderPlaced = delivery.json()?;
    if order.order_id.is_empty() {
        return Err(RuntimeError::message("an order without an id"));
    }
    if !order.paid {
        return Ok(Disposition::Retry);
    }
    tracing::info!(message_id = %delivery.message_id, order_id = %order.order_id, "order accepted");
    Ok(Disposition::Delete)
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::telemetry::logs::init()?;
    davidrs::queue::run(Arc::new(()), consume).await
}
