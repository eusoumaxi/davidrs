//! A streamed response outside the HTTP pipeline: a body whose producer is
//! owned by the response. No AWS resources.

use std::sync::Arc;
use std::time::Duration;

use davidrs::streaming::StreamBody;
use davidrs::{Context, RuntimeError};
use lambda_runtime::MetadataPrelude;

/// Streams three events. Dropping the body, when the client leaves, stops
/// the producer.
async fn stream(
    _app: Arc<()>,
    _payload: serde_json::Value,
    context: Context<()>,
) -> (MetadataPrelude, StreamBody) {
    let mut metadata = MetadataPrelude::default();
    metadata.headers.insert(
        "content-type",
        "text/event-stream".parse().expect("static header"),
    );
    let deadline = context.deadline().with_margin(Duration::from_millis(100));
    let body = StreamBody::spawn(4, deadline, |producer| async move {
        for index in 0..3 {
            if !producer.send(format!("data: {index}\n\n")).await {
                return;
            }
        }
    });
    (metadata, body)
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    davidrs::streaming::run(Arc::new(()), stream).await
}
