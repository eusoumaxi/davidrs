//! A streamed HTTP endpoint: JSON by default, server-sent events for
//! `Accept: text/event-stream`, CORS for one browser origin. No AWS resources.

use std::sync::Arc;
use std::time::Duration;

use davidrs::http::stream::{self, Cors, StreamApi, StreamRequest, StreamResponse};
use davidrs::http::{Failure, PlainErrors, Public, StatusCode};
use davidrs::{Context, RuntimeError};

/// Counts to three: one JSON document, or one event per step.
///
/// The producer keeps a second of the invocation back, so the stream ends
/// cleanly before Lambda's own timeout.
async fn count(
    _app: Arc<()>,
    request: StreamRequest,
    context: Context<()>,
) -> Result<StreamResponse, Failure> {
    if !request.wants_events() {
        return Ok(stream::json(
            StatusCode::OK,
            &serde_json::json!({"count": 3}),
        ));
    }
    let deadline = context.deadline().with_margin(Duration::from_secs(1));
    Ok(stream::events(deadline, |events| async move {
        for n in 1..=3 {
            let step = n.to_string();
            let frame = stream::sse_frame(Some(&step), Some("count"), &step);
            if events.send(frame).await.is_err() {
                return;
            }
        }
    }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("stream-http-example")?;
    StreamApi::new("count", Public, PlainErrors)
        .cors(Cors::new(vec!["http://localhost:3000".to_owned()]))
        .run(Arc::new(()), count)
        .await
}
