//! `mcp::Server::run` driven through one real invocation.
//!
//! A local server plays the Lambda Runtime API: it answers the first `/next`
//! with an HTTP API event, holds later ones open, and hands the test the
//! response the loop posts back. The loop reads its endpoint from process
//! environment variables, so this file is its own test binary with one test.
#![cfg(feature = "mcp")]

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use davidrs::http::Public;
use davidrs::mcp::{Server, PROTOCOL_VERSION};
use serde_json::{json, Value};
use support::server::{self, Recorded};
use tokio::sync::mpsc;

const REQUEST_ID: &str = "8476a536-e9f4-11e8-9739-2dfe598c3fcd";

/// The `/next` answer: the event and the headers Lambda sends with it.
fn invocation(event: &Value) -> server::Reply {
    let deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        + Duration::from_secs(30);
    let mut response = server::reply(200, "application/json", event.to_string());
    let headers = [
        ("lambda-runtime-aws-request-id", REQUEST_ID.to_owned()),
        (
            "lambda-runtime-deadline-ms",
            deadline.as_millis().to_string(),
        ),
        (
            "lambda-runtime-invoked-function-arn",
            "arn:aws:lambda:us-east-1:123456789012:function:mcp".to_owned(),
        ),
    ];
    for (name, value) in headers {
        let value = hyper::header::HeaderValue::from_str(&value).expect("header value");
        response.headers_mut().insert(name, value);
    }
    response
}

#[tokio::test]
async fn the_lambda_loop_serves_the_protocol() {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "ping",
        "params": { "_meta": {
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}
        } }
    });
    let event = json!({
        "version": "2.0",
        "rawPath": "/",
        "rawQueryString": "",
        "headers": {
            "content-type": "application/json",
            "mcp-protocol-version": PROTOCOL_VERSION,
            "mcp-method": "ping"
        },
        "requestContext": {
            "http": { "method": "POST", "path": "/", "sourceIp": "203.0.113.9" },
            "requestId": "r1",
            "stage": "$default",
            "timeEpoch": 0
        },
        "body": body.to_string(),
        "isBase64Encoded": false
    });
    let (sender, mut posted) = mpsc::unbounded_channel::<Recorded>();
    let delivered = Arc::new(AtomicBool::new(false));
    let runtime_api = server::Server::start(move |request: Recorded| {
        let next = request.path.ends_with("/invocation/next");
        let hold = next && delivered.swap(true, Ordering::SeqCst);
        let sender = sender.clone();
        let event = event.clone();
        async move {
            if hold {
                std::future::pending::<()>().await;
            }
            if next {
                return invocation(&event);
            }
            let _ = sender.send(request);
            server::reply(202, "application/json", r#"{"status":"OK"}"#)
        }
    })
    .await;
    for (name, value) in [
        ("AWS_LAMBDA_RUNTIME_API", runtime_api.authority()),
        ("AWS_LAMBDA_FUNCTION_NAME", "mcp".to_owned()),
        ("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128".to_owned()),
        ("AWS_LAMBDA_FUNCTION_VERSION", "$LATEST".to_owned()),
        ("AWS_LAMBDA_LOG_GROUP_NAME", "/aws/lambda/mcp".to_owned()),
        (
            "AWS_LAMBDA_LOG_STREAM_NAME",
            "2026/01/01/[$LATEST]0f1e".to_owned(),
        ),
    ] {
        std::env::set_var(name, value);
    }

    let lambda = Server::<(), _>::new("orders", "1.0.0", Public).run(Arc::new(()));
    let answer = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            ended = lambda => panic!("the loop ended before answering: {ended:?}"),
            answer = posted.recv() => answer.expect("the server is running"),
        }
    })
    .await
    .expect("the loop answered in time");

    assert_eq!(
        answer.path,
        format!("/2018-06-01/runtime/invocation/{REQUEST_ID}/response")
    );
    let response: Value = serde_json::from_slice(&answer.body).expect("a JSON response");
    assert_eq!(response["statusCode"], 200);
    let rpc: Value =
        serde_json::from_str(response["body"].as_str().expect("a body")).expect("JSON");
    assert_eq!(rpc["id"], 1);
    assert_eq!(rpc["result"]["resultType"], "complete");
}
