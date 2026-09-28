# Streaming

Two features cover a response that is written while the work runs. [`StreamApi`](crate::http::stream::StreamApi) (`http-stream`) is the HTTP pipeline: one handler answers JSON or server-sent events through a Lambda response stream. [`StreamBody`](crate::streaming::StreamBody) (`streaming`) is the body underneath it, and it owns the task that produces the bytes. `http-stream` enables `streaming`. Enable `streaming` alone only when you drive the Lambda response stream yourself and do not want the HTTP pipeline.

A buffered [`Api`](crate::http::Api) stops being the right tool past the gateway's integration timeout (30 seconds on an HTTP API) or past a 6 MB body. The front door is then a Function URL in `RESPONSE_STREAM` mode, or a REST API method with the `STREAM` integration. Dropping the body, which is what happens when the client goes away, cancels the producer. The producer's own deadline stops it even while a client keeps reading. Origin access control, the `x-amz-content-sha256` header, and why CORS for a streamed route is configured here rather than on the URL are in [Deployment](crate::guide::deployment) and [Security on AWS](crate::guide::aws_security).

## The streamed pipeline

### What it is

`StreamApi` is the streamed twin of [`Api`](crate::http::Api), for a Function URL in `RESPONSE_STREAM` mode or a REST API method with the `STREAM` integration. It reads the invocation payload as a request, runs a fixed sequence of checks, calls one handler and always answers: every failure, from any step, goes through the configured [`ErrorRenderer`](crate::http::ErrorRenderer) and is streamed like a success.

### Why it exists

A hand-written streamed handler tends to get the edges subtly wrong. It echoes whatever `Origin` arrived, so every site can read the response. It forgets `Vary`, so a cache hands an event stream to a JSON client. It answers a preflight with the handler's body, or runs until Lambda stops it, and the client sees a cut connection instead of an error. The pipeline fixes the order of these steps once.

### The steps

1. **Read.** The payload is parsed as an HTTP API or Function URL event, and as a REST API event with `apigw-rest`. Anything else is a `400` `ERROR_MALFORMED_REQUEST`.
2. **Prepare.** [`StreamApi::prepare`](crate::http::stream::StreamApi::prepare) changes the request before anything reads it: restoring a header that an edge function moved, for instance.
3. **CORS.** With a [`Cors`](crate::http::stream::Cors) allowlist, a request whose `Origin` is not on it is a `403` `ERROR_ORIGIN_NOT_ALLOWED`. A request without `Origin` (a server, `curl`) passes. `OPTIONS` is answered here as a preflight: `204`, no handler.
4. **Method.** A method outside [`StreamApi::methods`](crate::http::stream::StreamApi::methods) (`GET` by default) is a `405` `ERROR_METHOD_NOT_ALLOWED` with an `Allow` header.
5. **Negotiate.** [`negotiate`](crate::http::stream::negotiate) reads `Accept` and picks JSON or `text/event-stream`. An unreadable quality is a `400` `ERROR_INVALID_ACCEPT`; excluding both is a `406` `ERROR_NOT_ACCEPTABLE`. No `Accept`, or `*/*`, means JSON.
6. **Policy.** [`Policy::authorize`](crate::http::Policy::authorize) establishes the scope, or refuses.
7. **Handler.** It receives the application state, a [`StreamRequest`](crate::http::stream::StreamRequest) and a `Context<Scope>`, and returns a [`StreamResponse`](crate::http::stream::StreamResponse).

Steps 5 to 7 run under the invocation deadline minus a margin, one second by default ([`StreamApi::margin`](crate::http::stream::StreamApi::margin)), because a streamed response still has to flush its last frames after the handler returns. A handler still running when the margin is reached gets a `504` `ERROR_TIMEOUT`.

Every response, success or failure, then gets `Vary: Origin, Accept`, the headers added by [`StreamApi::finalize`](crate::http::stream::StreamApi::finalize), and, for an allowed origin, the CORS headers. Its `Set-Cookie` headers, from the handler or the finalizer, then move to the stream's list of cookies, which is where a Lambda response stream carries them.

Decoding and admission are the handler's, not stages of the pipeline. A streamed endpoint often decides what to count only once it knows the caller (an anonymous request spends a budget a signed-in one does not), which a stage before the policy cannot express. The handler reads what it needs from the `StreamRequest`: `view()` for the bounded typed readers, `native()` for the request itself, `path()`, `representation()` and `wants_events()`, and `event()` for the payload exactly as delivered, for what the typed request re-derives differently, such as a Function URL's own decoded query map.

### How to use it

```rust,no_run
use std::sync::Arc;
use std::time::Duration;

use davidrs::http::stream::{self, Cors, StreamApi, StreamRequest, StreamResponse};
use davidrs::http::{Failure, HeaderValue, Method, PlainErrors, Public, StatusCode};
use davidrs::{Context, RuntimeError};
use serde_json::json;

struct App;

async fn progress(
    _app: Arc<App>,
    request: StreamRequest,
    context: Context<()>,
) -> Result<StreamResponse, Failure> {
    if !request.wants_events() {
        return Ok(stream::json(StatusCode::OK, &json!({"done": 3})));
    }
    let deadline = context.deadline().with_margin(Duration::from_secs(1));
    Ok(stream::events(deadline, |events| async move {
        for step in 1..=3 {
            let data = step.to_string();
            let frame = stream::sse_frame(Some(&data), Some("progress"), &data);
            if events.send(frame).await.is_err() {
                return;
            }
        }
    }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let cors = Cors::new(vec!["https://app.example.com".to_owned()])
        .expose_headers("x-request-id");
    StreamApi::new("progress", Public, PlainErrors)
        .methods(&[Method::GET, Method::POST])
        .cors(cors)
        .finalize(|invocation, headers| {
            if let Ok(id) = HeaderValue::from_str(&invocation.request_id) {
                headers.insert("x-request-id", id);
            }
        })
        .run(Arc::new(App), progress)
        .await
}
```

The producer gets the invocation deadline minus its own margin, so it stops with time left to end the stream cleanly. [`StreamApi::handle`](crate::http::stream::StreamApi::handle) runs one invocation without the Lambda loop, which is what a test calls.

Negotiation is also usable on its own:

```rust
use std::time::Duration;

use davidrs::http::stream::{negotiate, Cors, Representation};

assert_eq!(negotiate(None).unwrap(), Representation::Json);
assert_eq!(
    negotiate(Some("text/event-stream;q=0, */*")).unwrap(),
    Representation::Json
);
assert_eq!(
    negotiate(Some("text/*;q=0.2, text/event-stream;q=0.9, application/json;q=0.5")).unwrap(),
    Representation::EventStream
);

let cors = Cors::new(vec!["https://app.example.com".to_owned()])
    .allow_headers("authorization,content-type")
    .max_age(Duration::from_secs(600));
assert!(cors.allows("https://app.example.com"));
assert!(!cors.allows("https://app.example.com.evil.example"));
```

The most specific range decides each representation's quality, so `text/event-stream;q=0` excludes the stream even when `*/*` would accept it. The higher quality wins; JSON wins a tie.

### Use cases

- A browser app on another origin follows a long job through `EventSource`, while a script calls the same URL for one JSON summary.
- A search that finds results over several seconds sends each batch as it arrives instead of holding the whole answer.
- A Function URL that must refuse every origin but its own front end, without an API Gateway in front of it.

### What it does not do

- It is not a router: one `StreamApi` serves one operation.
- It does not decode the body or run an admission stage; the handler does.
- CORS origins match exactly: no wildcards, no patterns, and no `Access-Control-Allow-Credentials`, so a bearer token in `Authorization` works and a cookie does not. The list comes from configuration, never from the request.
- It serves two representations, JSON and `text/event-stream`, nothing else.

## Streamed bodies

A handler returns one of three bodies:

- [`stream::json`](crate::http::stream::json) sends one JSON document. A `204` sends no body.
- [`stream::events`](crate::http::stream::events) starts a producer that writes frames through an [`EventWriter`](crate::http::stream::EventWriter). The response owns the producer: dropping it cancels the producer, and the deadline passed in bounds it even while the client keeps reading.
- [`stream::from_response`](crate::http::stream::from_response) streams a buffered response, such as a rendered failure, with its status, headers and body unchanged.

[`EventWriter::send`](crate::http::stream::EventWriter::send) fails when the client is gone, or when a frame has waited more than a second for a slow reader; either way the producer should stop. The producer runs at most four frames ahead of the reader. [`sse_frame`](crate::http::stream::sse_frame) builds one frame: optional `id:` and `event:` lines, one `data:` line per line of data, and the blank line that ends it.

```rust
use std::time::Duration;

use davidrs::http::stream::{self, sse_frame, EVENT_STREAM};
use davidrs::http::{ErrorRenderer, Failure, PlainErrors, StatusCode};
use davidrs::Deadline;

# #[tokio::main]
# async fn main() {
let deadline = Deadline::after(Duration::from_secs(5));
let response = stream::events(deadline, |events| async move {
    for n in 1..=2 {
        if events.send(sse_frame(None, Some("tick"), &n.to_string())).await.is_err() {
            return;
        }
    }
});
assert_eq!(response.metadata_prelude.headers["content-type"], EVENT_STREAM);
let body = response.stream.collect().await.expect("body").to_bytes();
assert_eq!(body, "event: tick\ndata: 1\n\nevent: tick\ndata: 2\n\n");

let refused = Failure::new(StatusCode::TOO_MANY_REQUESTS, "SLOW_DOWN", "Slow down")
    .with_header("retry-after", "5");
let response = stream::from_response(PlainErrors.render(&refused));
assert_eq!(response.metadata_prelude.status_code, StatusCode::TOO_MANY_REQUESTS);
assert_eq!(response.metadata_prelude.headers["retry-after"], "5");
# }
```

`sse_frame` does not send keep-alive comments or `retry:` lines, and it does not handle `Last-Event-ID`; a handler that resumes a stream reads that header itself. `id` and `event` are written as one line each: a line break in either is dropped, so a value that came from the request cannot add a field or end the frame early.

## Owned producers

### What it is

[`StreamBody`](crate::streaming::StreamBody) is a stream of chunks that owns the task producing them. [`StreamBody::spawn`](crate::streaming::StreamBody::spawn) starts the producer as a task and keeps its `JoinHandle` and a cancellation token; the producer writes through a [`Producer`](crate::streaming::Producer).

### Why it exists

The usual way to stream spawns a task, keeps the receiving end of a channel, and drops the task's handle. Nothing can stop that task any more: when the client goes away it keeps calling upstream services and writing rows, and when it panics the stream simply ends and looks complete. `StreamBody` makes the body the owner, so dropping the body is enough.

### How it behaves

- **Cancellation on drop.** Dropping the body cancels the token and closes the channel. A cooperative producer sees [`Producer::cancelled`](crate::streaming::Producer::cancelled) resolve, [`should_stop`](crate::streaming::Producer::should_stop) turn true, or [`send`](crate::streaming::Producer::send) return `false`. After 50 ms its future is dropped anyway.
- **Its own deadline.** Lambda does not guarantee that a disconnected client stops the invocation, and a client that keeps reading could hold it open. The producer's deadline ends the stream with [`RuntimeError::DeadlineExceeded`](crate::RuntimeError::DeadlineExceeded) when it passes.
- **Failures are items.** [`Producer::fail`](crate::streaming::Producer::fail) ends the stream with an error, and a panic surfaces as a final error item instead of a clean end. The panic's message is not passed on.
- **Backpressure.** The capacity given to `spawn` is how far the producer may run ahead; past it, `send` waits for the reader.
- [`StreamBody::once`](crate::streaming::StreamBody::once) is a body of one chunk already in memory, or of none, with no task.
- [`StreamBody::shutdown`](crate::streaming::StreamBody::shutdown) cancels the producer and waits for it, for when it must be done before you go on.

### How to use it

```rust
use std::time::Duration;

use davidrs::streaming::StreamBody;
use davidrs::Deadline;
use futures_util::StreamExt as _;

async fn fetch_page(page: u32) -> String {
    format!("page {page}\n")
}

# #[tokio::main]
# async fn main() {
let deadline = Deadline::after(Duration::from_secs(5));
let mut body = StreamBody::spawn(4, deadline, |producer| async move {
    let mut page = 0;
    while !producer.should_stop() {
        page += 1;
        let rows = tokio::select! {
            () = producer.cancelled() => return,
            rows = fetch_page(page) => rows,
        };
        if !producer.send(rows).await {
            return;
        }
    }
});
assert_eq!(body.next().await.expect("chunk").expect("page"), "page 1\n");
body.shutdown().await;
# }
```

`select!` on `cancelled()` abandons a slow upstream call as soon as the body goes away, instead of finishing it first. A Lambda that streams without the HTTP pipeline returns a `lambda_runtime::MetadataPrelude` and a `StreamBody` from [`streaming::run`](crate::streaming::run); name the prelude through your own `lambda_runtime = "1"` dependency.

### Use cases

- An export written page by page from a paginated upstream, stopped as soon as the client leaves.
- A report whose producer must not outlive its budget even when the client keeps the connection open.
- An image or document already rendered in memory, sent with `once`.

### What it does not do

- It does not roll back what the producer already did: a write to another service stays written, so it needs idempotency or reconciliation.
- Cancellation is cooperative for 50 ms, then the future is dropped at its next await point; code after that point does not run.
- It does not retry or restart a producer, and it has one reader.
