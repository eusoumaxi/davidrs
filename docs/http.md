# HTTP

[`Api`](crate::http::Api) serves one route from API Gateway (HTTP API, or REST API with the `apigw-rest` feature) or a Lambda Function URL. It is configured once with an operation name, a [`Policy`](crate::http::Policy) and an [`ErrorRenderer`](crate::http::ErrorRenderer); every invocation then runs the same fixed pipeline, under one deadline, and every failure leaves through the same renderer. The handler receives the shared state, a decoded input and a [`Context`](crate::Context), and returns a value or a [`Failure`](crate::http::Failure).

## Why it exists

A Lambda behind API Gateway is easy to write and easy to get subtly wrong:

- **A 500 leaks internals.** `format!("{error}")` in an error body sends a table name, an address or a provider message to the client. Here every 5xx renders a fixed message, however the failure was built.
- **A success answers 200 with a broken body.** Serialization that fails after the status is chosen sends an empty or truncated `200`. Here serialization is a step of the pipeline, and its failure is a rendered `500`.
- **The rate limit reads the body first.** A limiter that runs after decoding pays for parsing every refused request. Here admission runs before anything is parsed.
- **The body is unbounded.** A decoder that reads whatever arrives can be handed megabytes. Here every decoder, including one that reads raw bytes, sits behind a body limit.
- **Lambda's timeout answers for you.** A handler still awaiting when the function is stopped leaves the client a gateway error with no body. Here the pipeline stops 100 ms early and renders a `504`.
- **Headers go missing on the error path.** Cache headers set in the success path, or rate-limit headers set before a refusal, are easy to lose. Here a finalizer sees every response, and admission headers reach successes and failures alike.

## The pipeline

Each invocation runs these steps in order:

| Step | Runs | Produces |
| --- | --- | --- |
| 1. admission | async, before any parsing | headers for the response, or a failure |
| 2. decode | sync, after the body limit | the handler's input, or a `400`, `413` or `415` |
| 3. policy | async | the scope the handler runs with, or a `401` or `403` |
| 4. handler | async | a value that implements [`IntoResponse`](crate::http::IntoResponse), or a failure |
| 5. serialization | sync | the response, or a `500` |

Steps 1 to 5 share the invocation deadline minus a 100 ms reserve; when it runs out the pending step is dropped and a `504` (`ERROR_TIMEOUT`) is rendered. A failure from any step is logged with its code and kind only, never its message, then rendered. The finalizer set with [`Api::finalize`](crate::http::Api::finalize) runs last, on every response.

A complete function is one expression in `main`:

```rust,no_run
use std::sync::Arc;

use davidrs::http::{Api, Failure, HeaderValue, Json, PlainErrors, Public, Request};
use davidrs::{Context, RuntimeError};
use serde::{Deserialize, Serialize};

/// The shared state, built once per process.
struct App {
    prefix: String,
}

#[derive(Deserialize)]
struct NewItem {
    name: String,
}

#[derive(Serialize)]
struct Item {
    id: String,
}

async fn create(app: Arc<App>, input: NewItem, _context: Context<()>) -> Result<Json<Item>, Failure> {
    Ok(Json(Item { id: format!("{}{}", app.prefix, input.name) }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let app = Arc::new(App { prefix: "item-".to_owned() });
    Api::new("create-item", Public, PlainErrors)
        .body_limit(64 * 1024)
        .finalize(|_invocation, response| {
            response.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
        })
        .run(app, |request: &Request<'_>| request.json::<NewItem>(), create)
        .await
}
```

[`Api::run`](crate::http::Api::run) starts the Lambda loop. [`Api::handle`](crate::http::Api::handle) runs one request and returns the response, which is how a test drives a route without the runtime. A request built by hand has no Lambda context; it gets an empty request id and Lambda's maximum budget.

```rust
use std::sync::Arc;

use davidrs::http::{Api, Body, Failure, Json, PlainErrors, Public, Request, StatusCode};
use davidrs::Context;

async fn double(_app: Arc<()>, n: u32, _context: Context<()>) -> Result<Json<u32>, Failure> {
    Ok(Json(n * 2))
}

# #[tokio::main(flavor = "current_thread")]
# async fn main() {
let api = Api::new("double", Public, PlainErrors);
let decode = |request: &Request<'_>| request.json::<u32>();

let request = lambda_http::http::Request::builder()
    .method("POST")
    .uri("https://example.com/double")
    .body(Body::Text("21".to_owned()))
    .unwrap();
let response = api.handle(Arc::new(()), request, &decode, &double).await;
assert_eq!(response.status(), StatusCode::OK);
assert_eq!(response.body(), &Body::Text("42".to_owned()));

let garbage = lambda_http::http::Request::builder()
    .method("POST")
    .uri("https://example.com/double")
    .body(Body::Text("twenty-one".to_owned()))
    .unwrap();
let response = api.handle(Arc::new(()), garbage, &decode, &double).await;
assert_eq!(response.status(), StatusCode::BAD_REQUEST);
# }
```

Use it for any request–response endpoint behind API Gateway or a Function URL: a create or update that takes a JSON body, a lookup by path parameter, a listing with query filters, a webhook whose signature a policy checks over the raw bytes.

It does not route. One `Api` is one operation; several operations are several functions, or a decoder that dispatches on [`Request::method`](crate::http::Request::method) when they truly belong together. There is no middleware stack: the extension points are admission, policy, renderer and finalizer, each with one job. Serialization cannot be interrupted, so keep response values bounded. For streamed responses, server-sent events and CORS, use the `http-stream` feature's `StreamApi`.

## Reading the request

The decoder and the policy receive a [`Request`](crate::http::Request), a borrowed view of the native request that carries the body limit. Every typed read returns a [`Failure`](crate::http::Failure) of kind `Decode` instead of panicking:

| Read | Returns | Refuses with |
| --- | --- | --- |
| [`path`](crate::http::Request::path) | path parameters as `T` | `400 ERROR_INVALID_PATH` |
| [`query`](crate::http::Request::query) | the query string as `T` | `400 ERROR_INVALID_QUERY` |
| [`query_pairs`](crate::http::Request::query_pairs) | every `(name, value)`, repetitions kept | — |
| [`json`](crate::http::Request::json) | the body as `T` | `413`, `415`, `400 ERROR_MALFORMED_BODY` |
| [`json_limited`](crate::http::Request::json_limited) | the same, under a tighter limit | the same |
| [`json_text`](crate::http::Request::json_text) | the body as UTF-8, for your own decoder | `413`, `415`, `400` |
| [`raw_body`](crate::http::Request::raw_body) | the exact bytes, for a signature | — |
| [`media_type`](crate::http::Request::media_type) | `Content-Type`, lowercased, without parameters | — |
| [`source_ip`](crate::http::Request::source_ip) | the gateway's `sourceIp`, or `"unknown"` | — |

A JSON body with no `Content-Type` is accepted, because many clients send JSON without declaring it; a body that declares another media type is a `415` and is never parsed. An empty body is a `400`. The body limit is 1 MiB ([`DEFAULT_BODY_LIMIT`](crate::http::DEFAULT_BODY_LIMIT)) unless [`Api::body_limit`](crate::http::Api::body_limit) sets another, and the pipeline checks it before the decoder runs.

```rust
use davidrs::http::{Body, Request};
use serde::Deserialize;

#[derive(Deserialize)]
struct Filter {
    status: String,
    page: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Note {
    text: String,
}

let native = lambda_http::http::Request::builder()
    .method("POST")
    .uri("https://example.com/orders/notes?status=open&page=2")
    .header("content-type", "application/json; charset=utf-8")
    .body(Body::Text(r#"{"text":"call back"}"#.to_owned()))
    .unwrap();
let request = Request::new(&native);

let filter: Filter = request.query().unwrap();
assert_eq!((filter.status.as_str(), filter.page), ("open", Some(2)));
assert_eq!(request.json::<Note>().unwrap().text, "call back");
assert_eq!(request.media_type().as_deref(), Some("application/json"));

let tight = request.json_limited::<Note>(8).unwrap_err();
assert_eq!(tight.status().as_u16(), 413);
```

Reach for [`json_text`](crate::http::Request::json_text) when an application decoder needs the text, for [`raw_body`](crate::http::Request::raw_body) when a policy verifies an HMAC over the exact bytes, for [`query_pairs`](crate::http::Request::query_pairs) when a list arrives as `?tag=a&tag=b`, and for [`source_ip`](crate::http::Request::source_ip) as a rate-limit key.

`source_ip` ignores `X-Forwarded-For` on purpose: trusting a forwarded header needs a proxy policy of the application's own, and without one any caller could choose its own rate-limit key. The view never copies the body and never awaits; for anything it does not expose, [`Request::native`](crate::http::Request::native) returns the underlying request.

## Answering

A handler returns any [`IntoResponse`](crate::http::IntoResponse):

| Value | Response |
| --- | --- |
| [`Json(value)`](crate::http::Json) | `200`, `application/json`, the serialized value |
| [`NoContent`](crate::http::NoContent) | `204`, no body |
| `(StatusCode, T)` | `T`'s response with that status |
| `(StatusCode, HeaderMap, T)` | the same, with those headers set over `T`'s |
| `Option<T>` | `T`'s response, or `404 ERROR_NOT_FOUND` for `None` |
| [`HttpResponse`](crate::http::HttpResponse) | itself, unchanged |

Conversion returns a `Result`: when a value cannot be serialized, the result is a `500 FAULT_SERIALIZATION` of kind `Serialization`, and a status chosen in a tuple cannot hide it.

```rust
use davidrs::http::{HeaderMap, HeaderValue, IntoResponse, Json, NoContent, StatusCode};

let mut headers = HeaderMap::new();
headers.insert("location", HeaderValue::from_static("/orders/7"));
let created = (StatusCode::CREATED, headers, Json(serde_json::json!({"id": 7})))
    .into_response()
    .unwrap();
assert_eq!(created.status(), StatusCode::CREATED);
assert_eq!(created.headers()["location"], "/orders/7");

let missing = Option::<NoContent>::None.into_response().unwrap_err();
assert_eq!(missing.status(), StatusCode::NOT_FOUND);
```

Use `Option` for a lookup by id, a tuple for `201 Created` with a `Location`, `NoContent` for a delete, and a built [`HttpResponse`](crate::http::HttpResponse) for a CSV or any body that is not JSON. There is no content negotiation and no compression: a handler that needs them builds the response itself.

## Failures

A [`Failure`](crate::http::Failure) is what a handler returns when it cannot succeed. It carries:

| Part | Rendered | Purpose |
| --- | --- | --- |
| status | yes | the HTTP status |
| code | yes | a stable identifier clients can match on |
| message | 4xx only | what the client may read |
| detail | never | text for a deliberate diagnostic |
| kind | never | the step that failed: admission, decode, policy, handler, serialization, deadline |
| headers | yes | `Retry-After`, `Allow`, rate-limit fields |

A renderer can read the message only through [`public_message`](crate::http::Failure::public_message), which returns [`INTERNAL_MESSAGE`](crate::http::INTERNAL_MESSAGE) for every 5xx. `Display` shows the same thing, so an accidental `{failure}` in a body cannot leak either. [`internal_detail`](crate::http::Failure::internal_detail) returns the unredacted message and the detail for a log line you write on purpose; the pipeline's own log records only the operation, request id, code and kind.

```rust
use davidrs::http::{Failure, FailureKind, StatusCode, INTERNAL_MESSAGE};

let closed = Failure::new(StatusCode::CONFLICT, "ERROR_ORDER_CLOSED", "The order is closed")
    .with_detail("order 7 closed at 12:00");
assert_eq!(closed.public_message(), "The order is closed");
assert_eq!(closed.kind(), FailureKind::Handler);

let upstream = Failure::new(StatusCode::BAD_GATEWAY, "FAULT_UPSTREAM", "connection reset by 10.0.0.4");
assert_eq!(upstream.public_message(), INTERNAL_MESSAGE);
assert_eq!(upstream.to_string(), "502 InternalServerError");
assert!(upstream.internal_detail().contains("10.0.0.4"));

let io = std::io::Error::other("disk full");
let stored = Failure::from_error("FAULT_STORE", &io);
assert_eq!(stored.status(), StatusCode::INTERNAL_SERVER_ERROR);
assert_eq!(stored.internal_detail(), "disk full");

let busy = Failure::new(StatusCode::SERVICE_UNAVAILABLE, "FAULT_BUSY", "Try later")
    .with_header("retry-after", "5");
assert_eq!(busy.headers()[0].1, "5");
```

Build a 4xx with [`Failure::new`](crate::http::Failure::new) and a message the client can act on. Build a 5xx with [`Failure::internal`](crate::http::Failure::internal) or [`Failure::from_error`](crate::http::Failure::from_error), which keeps an error's whole source chain as the detail, or convert a [`RuntimeError`](crate::RuntimeError) with `?`. A header whose name or value is invalid is dropped instead of panicking.

A `Failure` does not log itself and does not decide whether a request may be retried: that is the status and the operation's own contract.

### Error catalogs

An [`ErrorCatalog`](crate::http::ErrorCatalog) declares a service's failures in one place, each an [`ErrorDefinition`](crate::http::ErrorDefinition) with a code, a status and a message. A `const` slice of definitions is a catalog; a code it does not know becomes a `500 FAULT_UNHANDLED` rather than a panic.

```rust
use davidrs::http::{ErrorCatalog, ErrorDefinition, StatusCode};

const ERRORS: &[ErrorDefinition] = &[
    ErrorDefinition::new("ERROR_ORDER_NOT_FOUND", StatusCode::NOT_FOUND, "The order does not exist"),
    ErrorDefinition::new("ERROR_ORDER_CLOSED", StatusCode::CONFLICT, "The order is closed"),
];

let failure = ERRORS.failure("ERROR_ORDER_CLOSED");
assert_eq!(failure.status(), StatusCode::CONFLICT);
assert_eq!(ERRORS.failure("ERROR_TYPO").status(), StatusCode::INTERNAL_SERVER_ERROR);
```

Use a catalog when clients document your error codes, or when several functions share one vocabulary. The pipeline never requires one, and implementing the trait over a map or a generated table works the same way.

## Renderers

An [`ErrorRenderer`](crate::http::ErrorRenderer) turns a failure into the response the client sees. It cannot fail: whatever it builds is what goes on the wire. Two are included:

- [`PlainErrors`](crate::http::PlainErrors) writes `{"errorCode": "...", "errorMessage": "..."}` as `application/json`.
- [`ProblemErrors`](crate::http::ProblemErrors), with the `problem` feature, writes RFC 9457 `application/problem+json`: `status`, a `title` from the status, and the public message as `detail`. [`with_type_base`](crate::http::ProblemErrors::with_type_base) turns the code into the `type` URI; without it `type` is omitted, which means `about:blank`.

Both copy the failure's headers onto the response.

```rust
use davidrs::http::{Body, ErrorRenderer, Failure, PlainErrors, ProblemErrors, StatusCode};

let failure = Failure::new(StatusCode::NOT_FOUND, "ERROR_NOT_FOUND", "No such order");

let plain = PlainErrors.render(&failure);
assert_eq!(
    plain.body(),
    &Body::Text(r#"{"errorCode":"ERROR_NOT_FOUND","errorMessage":"No such order"}"#.to_owned())
);

let problem = ProblemErrors::default()
    .with_type_base("https://example.com/errors/")
    .render(&failure);
assert_eq!(problem.headers()["content-type"], "application/problem+json");
let Body::Text(text) = problem.body() else { unreachable!() };
let json: serde_json::Value = serde_json::from_str(text).unwrap();
assert_eq!(json["type"], "https://example.com/errors/ERROR_NOT_FOUND");
assert_eq!(json["detail"], "No such order");
```

When clients expect another shape, write your own. [`literal`](crate::http::literal) builds the response from a status, a media type and a body you already serialized; if the media type is not a valid header value it returns an empty `500` rather than a half-built response. Read the message through `public_message`, and copy [`headers`](crate::http::Failure::headers) so a `429` keeps its `Retry-After`:

```rust
use davidrs::http::{literal, ErrorRenderer, Failure, HttpResponse, StatusCode};

struct XmlErrors;

impl ErrorRenderer for XmlErrors {
    fn render(&self, failure: &Failure) -> HttpResponse {
        let body = format!("<error code=\"{}\">{}</error>", failure.code(), failure.public_message());
        let mut response = literal(failure.status(), "application/xml", body);
        for (name, value) in failure.headers() {
            response.headers_mut().insert(name.clone(), value.clone());
        }
        response
    }
}

let response = XmlErrors.render(&Failure::internal("FAULT_STORE", "table unreachable"));
assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
```

The renderer owns the envelope and nothing else: it cannot change which step failed, and it never sees the success path. Headers every response needs belong in the finalizer.

## Policies and admission

A [`Policy`](crate::http::Policy) decides who is calling and returns a `Scope`, the authority the handler runs with. The scope is evidence: it exists only because the policy succeeded, so keep its fields private and construct it nowhere else. The handler reads it from [`Context::scope`](crate::Context::scope) and never re-reads a header. [`Public`](crate::http::Public) admits everyone with the scope `()`.

```rust
use std::sync::Arc;

use davidrs::http::{Api, Body, Failure, Json, PlainErrors, Policy, Request, StatusCode};
use davidrs::{Context, Invocation};

/// The caller a request was authorized for.
pub struct Caller {
    user: String,
}

/// Admits the users on an allowlist, named by a header a gateway authorizer sets.
struct Allowlist(Vec<String>);

impl Policy for Allowlist {
    type Scope = Caller;

    async fn authorize(&self, request: &Request<'_>, _: &Invocation) -> Result<Caller, Failure> {
        let user = request.header("x-user").unwrap_or_default();
        if !self.0.iter().any(|allowed| allowed == user) {
            return Err(Failure::new(StatusCode::FORBIDDEN, "ERROR_FORBIDDEN", "Not allowed"));
        }
        Ok(Caller { user: user.to_owned() })
    }
}

async fn whoami(_app: Arc<()>, _input: (), context: Context<Caller>) -> Result<Json<String>, Failure> {
    Ok(Json(context.scope().user.clone()))
}

# #[tokio::main(flavor = "current_thread")]
# async fn main() {
let api = Api::new("whoami", Allowlist(vec!["ada".to_owned()]), PlainErrors);
let request = lambda_http::http::Request::builder()
    .uri("https://example.com/whoami")
    .header("x-user", "grace")
    .body(Body::Empty)
    .unwrap();
let response = api.handle(Arc::new(()), request, &|_: &Request<'_>| Ok(()), &whoami).await;
assert_eq!(response.status(), StatusCode::FORBIDDEN);
# }
```

The policy runs after decoding, so a policy that verifies a signature can read [`raw_body`](crate::http::Request::raw_body), and a malformed request is refused before any key is fetched. Token verification itself is the `auth` feature's job; the policy decides what a verified token may do.

An [`Admission`](crate::http::Admission) runs first, before the body is read, and either admits the request with headers for the response or refuses it. [`AdmitAll`](crate::http::AdmitAll) is the default. Use admission for rate limits, quotas and maintenance switches: anything that must refuse cheaply. It cannot see the decoded input or the caller's scope; a limit that depends on who the caller is belongs in the policy or the handler.

### Rate-limit headers

A rate limiter reports its budget with the IETF `RateLimit-Policy` and `RateLimit` header fields. [`RateLimit`](crate::http::RateLimit) formats them from a [`RateLimitConfig`](crate::http::RateLimitConfig), the requests left and when the window resets, whatever store keeps the counter; it stores nothing itself. An admission returns [`RateLimit::headers`](crate::http::RateLimit::headers), and the pipeline attaches them to the response whether the request then succeeds or fails; on a refusal, attach them to the `429` itself.

```rust
use std::time::{Duration, SystemTime};

use davidrs::http::{Admission, Failure, RateLimit, RateLimitConfig, Request, StatusCode};
use davidrs::Invocation;

const SEARCH: RateLimitConfig = RateLimitConfig::new("search", 60, Duration::from_secs(300));

/// A limiter whose store is left out: every caller has 7 requests left.
struct PerAddress;

impl PerAddress {
    /// Spends one request for `key`; returns what is left and when the window
    /// resets.
    async fn spend(&self, _key: &str) -> (i64, SystemTime) {
        (7, SystemTime::now() + Duration::from_secs(90))
    }
}

impl Admission for PerAddress {
    async fn check(&self, request: &Request<'_>, _: &Invocation) -> Result<Vec<(String, String)>, Failure> {
        let (remaining, resets_at) = self.spend(&request.source_ip()).await;
        let limit = RateLimit::new(&SEARCH, remaining, resets_at);
        if limit.is_exceeded() {
            return Err(Failure::new(StatusCode::TOO_MANY_REQUESTS, "ERROR_RATE_LIMITED", "Too many requests")
                .with_headers(limit.headers()));
        }
        Ok(limit.headers())
    }
}

# #[tokio::main(flavor = "current_thread")]
# async fn main() {
let native = lambda_http::http::Request::builder().body(davidrs::http::Body::Empty).unwrap();
let invocation = davidrs::Invocation::new("r", davidrs::Deadline::in_from_now(Duration::from_secs(1)));
let headers = PerAddress.check(&Request::new(&native), &invocation).await.unwrap();
assert_eq!(headers[0], ("RateLimit-Policy".to_owned(), "\"default\";q=60;w=300".to_owned()));
assert!(headers[1].1.starts_with("\"default\";r=7;t="));
# }
```

## The pipeline's own codes

Most codes come from your service. The pipeline refuses some requests before a handler runs, and those failures use the constants in [`codes`](crate::http::codes):

| Code | Status | Raised when |
| --- | --- | --- |
| `ERROR_INVALID_PATH` | 400 | path parameters do not deserialize |
| `ERROR_INVALID_QUERY` | 400 | the query string does not deserialize |
| `ERROR_MALFORMED_BODY` | 400 | the body is empty, not UTF-8 or not valid JSON |
| `ERROR_INVALID_BODY` | 400 | the body fails validation |
| `ERROR_BODY_TOO_LARGE` | 413 | the body is over the limit |
| `ERROR_UNSUPPORTED_MEDIA_TYPE` | 415 | the body declares a media type other than JSON |
| `ERROR_NOT_FOUND` | 404 | a handler returned `None` |
| `ERROR_TIMEOUT` | 504 | the deadline ran out |
| `FAULT_SERIALIZATION` | 500 | the success value did not serialize |
| `ERROR_LIMIT_EXCEEDED`, `FAULT_UNHANDLED` | 500 | a [`RuntimeError`](crate::RuntimeError) was converted, or a catalog did not know a code |

The streamed pipeline adds its own for CORS, methods and `Accept`; [`codes`](crate::http::codes) lists them all. A code's prefix is not a retry instruction: `ERROR_TIMEOUT` is a `504`.

These spellings are defaults, not a contract you are stuck with. When your clients expect another vocabulary, map the codes in your renderer, which sees the whole failure:

```rust
use davidrs::http::{codes, literal, ErrorRenderer, Failure, HttpResponse, StatusCode};

struct SnakeCase;

impl ErrorRenderer for SnakeCase {
    fn render(&self, failure: &Failure) -> HttpResponse {
        let code = match failure.code() {
            codes::MALFORMED_BODY | codes::INVALID_BODY => "invalid_request",
            codes::TIMEOUT => "timeout",
            other => other,
        };
        let body = serde_json::json!({"code": code, "message": failure.public_message()}).to_string();
        literal(failure.status(), "application/json", body)
    }
}

let response = SnakeCase.render(&Failure::new(StatusCode::BAD_REQUEST, codes::MALFORMED_BODY, "Invalid request body"));
assert_eq!(response.status(), StatusCode::BAD_REQUEST);
```

## Validation with Garde

With the `validate` feature, [`Request::validated_json`](crate::http::Request::validated_json) decodes a JSON body and checks it with a [Garde](https://docs.rs/garde) `Validate` derive. A body that decodes but breaks a rule is a `400 ERROR_INVALID_BODY`, and the handler never runs, so it only ever sees values that hold.

```rust
use davidrs::http::{codes, Body, Request};

#[derive(Debug, serde::Deserialize, garde::Validate)]
#[serde(deny_unknown_fields)]
struct NewOrder {
    #[garde(length(min = 1, max = 64))]
    reference: String,
    #[garde(range(min = 1, max = 100))]
    quantity: u32,
}

let native = lambda_http::http::Request::builder()
    .body(Body::Text(r#"{"reference":"A-1","quantity":0}"#.to_owned()))
    .unwrap();
let failure = Request::new(&native).validated_json::<NewOrder>().unwrap_err();
assert_eq!(failure.code(), codes::INVALID_BODY);
```

Use it for field constraints a type cannot express: lengths, ranges, nested items with `#[garde(dive)]`. The failure says that validation failed, not which field: a client that needs every invalid field listed gets that from the [`schema`](crate::http::schema) checks, and a rule that needs context (the caller, a stored value) runs in the handler with `validate_with`. Garde's own default features (regex, email, URL, phone numbers) are not enabled.

## Problem details

With the `problem` feature, [`ProblemErrors`](crate::http::ProblemErrors) renders every failure as RFC 9457 `application/problem+json`, the format standard HTTP clients and API gateways recognise. It is a renderer like any other: pass it to [`Api::new`](crate::http::Api::new) and every failure, from admission to serialization, uses it. The public-message rule holds, so a 5xx `detail` is always `InternalServerError`. It writes no extension members: a client that needs more than `type`, `title`, `status` and `detail` needs a renderer of its own.
