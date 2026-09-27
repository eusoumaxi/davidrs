//! What [`StreamApi`] logs when a request fails.
//!
//! This installs a subscriber, and `tracing` caches which call sites are
//! enabled for the whole process, so it lives in its own test binary.
#![cfg(all(feature = "http-stream", feature = "logs"))]

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use davidrs::http::stream::{StreamApi, StreamRequest, StreamResponse};
use davidrs::http::{codes, Failure, PlainErrors, Public, StatusCode};
use davidrs::Context;
use lambda_runtime::LambdaEvent;
use serde_json::{json, Value};

/// Captures what a `fmt` subscriber writes.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn event(method: &str) -> LambdaEvent<Value> {
    let payload = json!({
        "version": "2.0",
        "rawPath": "/orders",
        "rawQueryString": "",
        "headers": {},
        "requestContext": {
            "http": { "method": method, "path": "/orders", "sourceIp": "203.0.113.9" },
            "requestId": "r1",
            "stage": "$default",
            "timeEpoch": 0
        },
        "isBase64Encoded": false
    });
    let mut context = lambda_runtime::Context::default();
    context.request_id = "req-1".to_owned();
    let due = SystemTime::now() + Duration::from_secs(30);
    context.deadline = due.duration_since(UNIX_EPOCH).expect("clock").as_millis() as u64;
    LambdaEvent::new(payload, context)
}

async fn broken(
    _app: Arc<()>,
    _request: StreamRequest,
    _context: Context<()>,
) -> Result<StreamResponse, Failure> {
    Err(Failure::internal("STORE_UNAVAILABLE", "token=secret-value"))
}

#[tokio::test]
async fn a_failure_is_logged_with_its_code_and_never_its_detail() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let api = StreamApi::new("orders", Public, PlainErrors);

    let failed = api.handle(Arc::new(()), event("GET"), &broken).await;
    let refused = api.handle(Arc::new(()), event("DELETE"), &broken).await;

    assert_eq!(
        failed.metadata_prelude.status_code,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        refused.metadata_prelude.status_code,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let logged = String::from_utf8(captured.0.lock().expect("log").clone()).expect("utf-8");
    assert!(logged.contains("STORE_UNAVAILABLE"), "{logged}");
    assert!(logged.contains(codes::METHOD_NOT_ALLOWED), "{logged}");
    assert!(!logged.contains("secret-value"), "{logged}");
}
