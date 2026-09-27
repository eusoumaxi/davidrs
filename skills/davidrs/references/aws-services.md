Calling AWS and other services from a davidrs function: SDK configuration, DynamoDB, EventBridge, Secrets Manager, outbound HTTP, bearer-token verification, caches, gzip, SHA-256, telemetry, and the least-privilege IAM each call needs.

# AWS and other services

`davidrs` keeps the AWS SDK for Rust and `reqwest` as they are and adds what every Lambda function gets wrong around them: configuration that fails the cold start, reads and batches that say what they did not do, per-entry publish outcomes, secrets that never reach an error, HTTP clients with limits, and token checks that fail closed. Keys, conditions and item shapes stay in the SDK's own builders.

## Features and manifest

| Capability | Feature | Add to the function |
| --- | --- | --- |
| `aws::sdk_config`, `aws::Trust` | `aws` (implied by the next three) | |
| DynamoDB: `dynamo::*` | `dynamo` | `aws-sdk-dynamodb` |
| EventBridge: `eventbridge::*` | `eventbridge` | `aws-sdk-eventbridge` |
| Secrets Manager: `secrets::*` | `secrets` | `aws-sdk-secretsmanager` |
| outbound HTTP: `client::*` | `client` | `reqwest`, only to name `reqwest::Client` |
| bearer tokens: `auth::*` | `auth` (implies `client`) | |
| `cache`, `compression`, `digest` | one feature each | |
| logs, EMF metrics, X-Ray traces | `logs`, `metrics`, `otel` (implies `logs`) | `tracing`, for your own spans |

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["http", "dynamo", "secrets", "logs"] }
aws-sdk-dynamodb = { version = "1", default-features = false }
aws-sdk-secretsmanager = { version = "1", default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

`default-features = false` on the SDK crates keeps their own HTTPS clients and TLS stacks out of the build: `sdk_config` supplies the HTTP client (rustls with `ring`) and the Tokio timer. `Deadline::run` needs a trigger feature (`runtime`, `http`, `queue`, `event`, …) or `aws`; `client` alone does not bring it. Enable only what the function uses, and check it alone with `cargo check -p <function>`: a workspace build unifies features and can hide a missing one.

## Rules

- Build clients, secrets, the token verifier and the HTTP client once in `main` and share them as `Arc<App>`, so a missing variable, secret or key set fails the cold start, not the first request.
- Secrets come from Secrets Manager, never from environment variables, and never reach a log line, a `Failure` message or detail, or a rate-limit key.
- Bound every read: `PageLimits` on queries and scans, `max_attempts` on batches, a byte cap on every HTTP body (never `reqwest::Client::new()`, `response.json()` or `response.bytes()`), a decoded-size cap on gzip.
- Never report a partial outcome as success: an incomplete page, unprocessed writes or keys, a rejected or unknown event. Checking only the `Result` of `BatchWriteItem`, `BatchGetItem` or `PutEvents` misses them.
- The tenant or owner of a query comes from the verified scope and stays in the key condition. A page token is a position, never a permission.
- Derive every budget from `context.deadline()` with `child` or `with_margin`; retries share it and never start a fresh timeout.
- Refuse with `Failure::new(4xx, code, message)`. Build a `5xx` with `Failure::internal` or `Failure::from_error`; a `RuntimeError` returned with `?` becomes one. A `5xx` renders a fixed message and keeps its cause as a detail that is never rendered.
- Map `reqwest` errors with `client::send_error` before anything can log them: a `reqwest::Error` prints its URL, which may hold a key.
- Behind an API Gateway JWT authorizer, `Access` reads the claims the gateway verified; verify tokens in the function only where no authorizer sits in front, and accept access tokens, never ID tokens.

## The pattern: build once, bound every read

A listing behind an API Gateway JWT authorizer (features `http`, `dynamo`, `secrets`, `logs`):

```rust
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_dynamodb::types::AttributeValue;
use davidrs::aws::{sdk_config, Trust};
use davidrs::http::access::{Access, Claims, Grant};
use davidrs::http::{codes, Api, Failure, Json, PlainErrors, Request, StatusCode};
use davidrs::dynamo::{self, CursorSecret, PageLimits};
use davidrs::{required_env, Context, RuntimeError};
use serde::Deserialize;

/// Everything built once per execution environment.
struct App {
    orders: aws_sdk_dynamodb::Client,
    table: String,
    cursors: CursorSecret,
}

/// The caller: the subject the gateway's JWT authorizer verified.
fn subject(claims: &Claims) -> Option<String> {
    claims.subject().map(str::to_owned)
}

/// The query string of `GET /orders?next=<token>`.
#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    next: Option<String>,
}

/// One page of the caller's orders. The partition comes from the verified
/// caller, never from the token, and the read stops 300 ms early so a partial
/// page is still answered.
async fn list_orders(
    app: Arc<App>,
    listing: Listing,
    context: Context<Grant<String, ()>>,
) -> Result<Json<serde_json::Value>, Failure> {
    let owner = format!("USER#{}", context.scope().caller());
    let deadline = context.deadline().with_margin(Duration::from_millis(300));
    let mut start = listing
        .next
        .as_deref()
        .and_then(|token| dynamo::decode_cursor_signed(token, &app.cursors));
    let page = dynamo::query_bounded(PageLimits::new(50, 5), deadline, |resume| {
        app.orders
            .query()
            .table_name(&app.table)
            .key_condition_expression("PK = :owner")
            .expression_attribute_values(":owner", AttributeValue::S(owner.clone()))
            .set_exclusive_start_key(resume.or_else(|| start.take()))
    })
    .await?;
    if page.next.is_none() && !page.is_complete() {
        let message = "Request deadline exceeded";
        return Err(Failure::new(StatusCode::GATEWAY_TIMEOUT, codes::TIMEOUT, message));
    }
    let next = page
        .next
        .map(|key| dynamo::encode_cursor_signed(key, &app.cursors))
        .transpose()?;
    let items = page
        .items
        .into_iter()
        .map(|item| dynamo::to_object(item, &["PK", "SK"]))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(serde_json::json!({ "items": items, "next": next })))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("list-orders")?;
    let config = sdk_config(Trust::NativeRoots)?;
    let secrets = aws_sdk_secretsmanager::Client::new(&config);
    let cursor_secret = davidrs::secrets::string(&secrets, &required_env("CURSOR_SECRET_ID")?).await?;
    let app = Arc::new(App {
        orders: aws_sdk_dynamodb::Client::new(&config),
        table: required_env("ORDERS_TABLE")?,
        cursors: CursorSecret::new(cursor_secret.as_bytes()),
    });
    Api::new("list-orders", Access::new(subject).require_caller(), PlainErrors)
        .run(app, |request: &Request<'_>| request.query::<Listing>(), list_orders)
        .await
}
```

The buffered HTTP pipeline answers `504` 100 ms before `context.deadline()`, so a helper given the full deadline never returns its partial outcome: give it a margin, as above. The fragments below are statements inside such a helper, which returns `Result<_, RuntimeError>`: `app` is the application state holding the clients a fragment names, `deadline` its budget, `context` the handler's `Context`, and `id`, `ids`, `name`, `items` or `compressed` its inputs.

## AWS configuration (`aws`)

`sdk_config(trust)` reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` and the optional `AWS_SESSION_TOKEN`, performs no network I/O, and fails with `RuntimeError::Configuration` naming a missing variable. There is no credential chain (no profiles, SSO, instance metadata or role assumption), credentials are read once, and no endpoint, retry or timeout variable is read.

- `Trust::NativeRoots` reads the operating system's store at the first connection: the default.
- `Trust::Pem(include_bytes!("../amazon-trust-services.pem"))` trusts only a bundle compiled into the binary, which shortens the cold start; most AWS endpoints chain to the Amazon Trust Services roots. A root missing from the bundle fails TLS until the next deploy, and a bundle without a valid certificate makes the SDK panic at the first connection, so make one real call after changing it.

Locally, export what Lambda injects: `eval "$(aws configure export-credentials --format env)"` and `export AWS_REGION=eu-west-1`. DynamoDB Local takes placeholder credentials (its access key and Region only name the database file) and an endpoint set on the service configuration:

```rust
use davidrs::aws::{sdk_config, Trust};
use davidrs::optional_env;

let config = sdk_config(Trust::NativeRoots)?;
let mut orders = aws_sdk_dynamodb::config::Builder::from(&config);
if let Some(endpoint) = optional_env("DYNAMODB_ENDPOINT") {
    orders = orders.endpoint_url(endpoint);
}
let orders = aws_sdk_dynamodb::Client::from_conf(orders.build());
```

Tests need no account: build the SDK client over an in-process HTTP client (`aws-smithy-http-client` with `test-util`), as the crate's [DynamoDB tests](https://github.com/eusoumaxi/davidrs/blob/main/tests/table.rs) do.

## DynamoDB (`dynamo`)

**Bounded reads.** `query_bounded(PageLimits::new(max_items, max_pages), deadline, |resume| builder)` calls the closure once per page with the key to start from (`None` first) and lowers each request's `Limit` to the items still wanted, so `Page::next` resumes exactly; `scan_bounded` is the same for scans. `Page::stopped_by` is `None` (read to the end), `Stop::Items` (a full page), `Stop::Pages` (pages came back short: a filter discarded items, or a page reached 1 MB) or `Stop::Deadline`. `next` is `None` both when the read is complete and when it stopped before its first page, so test `is_complete()`. A failed page request discards the pages already read and returns the error.

**Page tokens.** `encode_cursor_signed(key, &cursor_key)` writes the key's JSON and its HMAC-SHA256 in URL-safe base64; `decode_cursor_signed` returns `None` for an edited, forged or garbled token, which restarts at the first page. Build the `CursorSecret` from a secret read at cold start (32 random bytes). A signed token is tamper-proof, not secret; the unsigned `encode_cursor`/`decode_cursor` are readable and editable. Either way the tenant stays in the key condition; a scan has no partition boundary, so apply the tenant there as a filter.

**Batches.** `BatchWriteItem` and `BatchGetItem` succeed while returning work they did not do. `batch_write` sends `MAX_BATCH_WRITE_REQUESTS` (25) requests per call, sends returned work again first after a pause that doubles from 25 ms, and its `max_attempts` bounds the round trips of the whole call (60 requests need at least 3):

```rust
use aws_sdk_dynamodb::types::{AttributeValue, PutRequest, WriteRequest};
use davidrs::dynamo;

let requests = ids
    .iter()
    .map(|id| {
        let put = PutRequest::builder()
            .item("PK", AttributeValue::S(format!("ORDER#{id}")))
            .item("SK", AttributeValue::S("SUMMARY".to_owned()))
            .build()
            .map_err(|error| RuntimeError::other("building a put", error))?;
        Ok(WriteRequest::builder().put_request(put).build())
    })
    .collect::<Result<Vec<_>, RuntimeError>>()?;
let outcome = dynamo::batch_write(&app.orders, &app.table, requests, 8, deadline).await?;
if !outcome.is_complete() {
    let unsaved = outcome.unprocessed.len();
    return Err(RuntimeError::message(format!("{unsaved} of {} orders not saved", ids.len())));
}
```

`batch_get(client, table, keys, max_attempts, deadline, |batch| batch.consistent_read(true))` reads `MAX_BATCH_GET_KEYS` (100) keys per request, gives each batch `max_attempts`, and returns a `BatchGetOutcome`: `items` (a key with no item is absent) and `unprocessed` (the keys of the batch that ran out, then every later batch's). Keys must be distinct: the service rejects a batch that repeats one.

**Conditional writes and spans.** A refused condition is usually an answer, not a fault. `span` names your own call `DynamoDB.<operation>` for X-Ray's service graph, as the helpers do for theirs:

```rust
use aws_sdk_dynamodb::types::AttributeValue;
use davidrs::dynamo;
use tracing::Instrument as _;

let put = app
    .orders
    .put_item()
    .table_name(&app.table)
    .item("PK", AttributeValue::S(format!("ORDER#{id}")))
    .item("SK", AttributeValue::S("CLAIM".to_owned()))
    .condition_expression("attribute_not_exists(PK)")
    .send()
    .instrument(dynamo::span("PutItem", &app.table));
let claimed = match deadline.run(put).await? {
    Ok(_) => true,
    Err(error) if dynamo::is_conditional_failure(&error) => false,
    Err(error) => return Err(RuntimeError::other("claiming the order", error)),
};
```

`to_object(item, &["PK", "SK"])` returns an item as a JSON object without its physical keys. There is no mapper, repository, transaction or single-item helper: use the SDK builders and `serde_dynamo`.

## EventBridge (`eventbridge`)

`PutEvents` answers `200` while rejecting individual entries; here every entry has an explicit outcome. The detail is any `Serialize` value. Publish after the change is stored, and fail the invocation unless the entry was accepted, so a retry publishes again:

```rust
use davidrs::eventbridge::{self, EntryOutcome, EventRoute};

/// Where the event goes: a `const` next to the detail it carries.
const ORDER_SHIPPED: EventRoute = EventRoute {
    source: "example.orders",
    detail_type: "order.shipped",
};

let detail = serde_json::json!({ "orderId": id });
let outcome = eventbridge::publish(&app.events, &app.bus, &ORDER_SHIPPED, &detail, deadline).await?;
match outcome.entries.first() {
    Some(EntryOutcome::Accepted(_event_id)) => {}
    Some(EntryOutcome::Rejected { code, .. }) => {
        return Err(RuntimeError::message(format!("order.shipped rejected: {code}")));
    }
    _ => return Err(RuntimeError::message("order.shipped may not have been published")),
}
```

- `publish_batch(client, bus, &ROUTE, &details, deadline)` takes up to `MAX_ENTRIES` (10) details of one route; split larger sets with `chunks(MAX_ENTRIES)`. An empty slice makes no call.
- `PublishOutcome::entries` has one `EntryOutcome` per input, in order: `Accepted(event_id)`, `Rejected { code, message }`, or `Unknown` (the deadline passed or the service said nothing, so the event may have been published). `all_accepted()` and `failed_indices()` count `Unknown` as not accepted.
- `Err` means a detail that did not serialize, a call refused as a whole, or more than ten details (`RuntimeError::LimitExceeded`, nothing sent). No retries, no splitting and no size check: publishing again is the caller's decision, and consumers must be idempotent anyway.

## Secrets Manager (`secrets`)

`secrets::string(&client, name)` returns a secret's string value; `secrets::json::<T>(&client, name)` deserializes it into your type. Read them once in `main`, as the pattern does: the name comes from configuration, the value never does. Errors name the secret, never its value; a binary-only secret, or JSON of the wrong shape (reported by line and column only), is `RuntimeError::Configuration`. The current version is read once: after a rotation, new execution environments read the new value.

## Outbound HTTP (`client`)

`client::build(Limits::new(connect, request))` returns a plain `reqwest::Client` (`Limits::default()` is 2 s to connect and 10 s per request) with redirects off and the Mozilla roots compiled in; build it once and keep it in `App`. Each call sends and maps the transport error, decides what the status means, and reads the body under a cap:

```rust
use davidrs::client::{json_bounded, send_error};
use davidrs::RuntimeError;

#[derive(serde::Deserialize)]
struct Rate {
    currency: String,
    value: f64,
}

async fn fetch_rate(http: &reqwest::Client, key: &str, currency: &str) -> Result<Rate, RuntimeError> {
    let response = http
        .get("https://rates.example.com/latest")
        .query(&[("currency", currency)])
        .header("x-api-key", key)
        .send()
        .await
        .map_err(|error| send_error("fetching the rate", error))?;
    if !response.status().is_success() {
        let status = response.status();
        return Err(RuntimeError::message(format!("the rate service answered {status}")));
    }
    json_bounded(response, 16 * 1024).await
}
```

Bound the call by the invocation with a child budget, which never outlives its parent: `deadline.child(Duration::from_secs(2)).run(fetch_rate(&app.http, &app.rates_key, "EUR")).await?` yields the call's own `Result`. `json_bounded` and `read_bounded` refuse a declared `Content-Length` over the cap before reading, check every chunk, and return `RuntimeError::LimitExceeded`, never a truncated body; they read any status. There are no retries and no host allowlist: build URLs from configuration, never from request input.

## Bearer tokens (`auth`)

Verify tokens in the function only where no authorizer sits in front (Function URLs, MCP servers, direct invocations) or as a second check. With features `http` and `auth`, in `main`, and the pattern's `subject`:

```rust
use std::sync::Arc;

use davidrs::auth::{Verifier, VerifierConfig};
use davidrs::client::{self, Limits};
use davidrs::http::access::Access;
use davidrs::{list_env, required_env};

let issuer = required_env("TOKEN_ISSUER")?;
let config = VerifierConfig::new(issuer.clone(), format!("{issuer}/.well-known/jwks.json"))
    .with_audiences(list_env("TOKEN_AUDIENCES")?)
    .with_required_claim("token_use", "access");
let verifier = Verifier::load(client::build(Limits::default())?, config).await?;
let policy = Access::new(subject)
    .gateway_claims(false)
    .verify_bearer(Arc::new(verifier))
    .require_caller();
```

- `VerifierConfig` accepts no token until `with_audiences` is set; a token without `aud` is matched on `client_id`, as an API Gateway JWT authorizer does. `requiring` pins a claim (`token_use` is Amazon Cognito's). `without_audience_check` is only for an issuer that serves this application alone. `leeway` and `min_refresh_interval` (60 s each) are public fields.
- `Verifier::load` fetches the key set before returning, so a wrong URL fails the cold start; `Verifier::deferred` fetches on the first token.
- RS256 only. Tokens over `MAX_TOKEN_BYTES` (8 KiB) and key sets over `MAX_JWKS_BYTES` (64 KiB) are refused; an unknown `kid` causes at most one shared refresh per interval. `exp` is required and `nbf` honoured; `iat` is not checked, and there is no revocation.
- In a custom `Policy`, `verifier.verify(bearer(header)).await` returns `VerifiedClaims` (`subject`, `string`, `get`, `expires_at`, `all`) or a `VerifyError`: answer every variant with the same `401` and keep the reason in `with_detail`.

## Caches, gzip and digests (`cache`, `compression`, `digest`)

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::cache::Cache;
use davidrs::compression::gunzip_to_string_bounded;
use davidrs::digest::sha256_hex;

let prices: Cache<String, Arc<Vec<u32>>> = Cache::builder()
    .max_capacity(1_000)
    .time_to_live(Duration::from_secs(300))
    .build();
let list = prices
    .try_get_with(name.to_owned(), async { load_prices(name).await.map(Arc::new) })
    .await
    .map_err(|error| RuntimeError::other("loading prices", error))?;

let document = gunzip_to_string_bounded(&compressed, 256 * 1024)?;
let key = sha256_hex(document.as_bytes());
```

- `cache::Cache` is Moka's async cache, re-exported with its own API; build it with the application state. `get_with` and `try_get_with` run one load per key for concurrent misses; a failed `try_get_with` reaches every waiter and caches nothing. Each execution environment has its own copy, empty after a cold start: right for reference data and upstream tokens, wrong for rate limits, sessions or idempotency records.
- `compression::gzip` compresses; `gunzip_bounded` and `gunzip_to_string_bounded` fail with `LimitExceeded` past the cap on the decoded size, whatever the compressed size.
- `digest::sha256`, `sha256_hex` (lowercase, two digits per byte) and `hex`. Hash a canonical form; SHA-256 is neither a MAC nor a password hash.

## Telemetry (`logs`, `metrics`, `otel`)

- `let _telemetry = davidrs::telemetry::init("service")?;` comes first in `main`; keep the guard until `main` returns. A second call is an error. `RUST_LOG` takes a bare level (`debug`, `warn`), not directives.
- `telemetry::logs::timed_init("component", future)` logs a startup step's `init_ms` and `success`, never its value or error.
- Every pipeline opens a `lambda.invocation` span carrying the request id; your own `tracing` spans nest under it.
- `otel`: `init` also exports each finished span to the sandbox's X-Ray agent over UDP. Set the function's tracing mode to Active. Every span is exported (the incoming `Sampled` flag is not read), and outbound calls carry no trace header unless you set `X-Amzn-Trace-Id`.
- EMF metrics (`metrics`): one document per call, printed as one whole line with `println!`, never through `tracing`, whose prefix stops CloudWatch reading it. At most 100 metrics and 30 dimensions; extras are ignored rather than losing the document. Keep request ids in properties: as a dimension, each value becomes its own metric.

```rust
use std::time::SystemTime;

use davidrs::telemetry::{Metrics, Unit};

let line = Metrics::new("Orders")
    .dimension("Operation", "list-orders")
    .metric("Items", items as f64, Unit::Count)
    .property("requestId", context.invocation().request_id.as_str())
    .to_json(SystemTime::now())?;
println!("{line}");
```

## Least-privilege IAM

One role per function, with exactly these actions on exactly the resources it names:

| Call | Actions | Resource |
| --- | --- | --- |
| `dynamo::query_bounded`, `scan_bounded` | `dynamodb:Query`, `dynamodb:Scan` | the table's ARN; `…:table/<name>/index/<index>` for an index |
| `dynamo::batch_write`, `batch_get` | `dynamodb:BatchWriteItem`, `dynamodb:BatchGetItem` | the table |
| your own item calls | `dynamodb:GetItem`, `dynamodb:PutItem`, `dynamodb:UpdateItem`, … | the table |
| `http::rate_limit::DynamoWindow` | `dynamodb:UpdateItem` | the counter table |
| `eventbridge::publish`, `publish_batch` | `events:PutEvents` | `arn:aws:events:<region>:<account>:event-bus/<name>`; narrow with the `events:source` and `events:detail-type` condition keys |
| `secrets::string`, `secrets::json` | `secretsmanager:GetSecretValue` | the secret; plus `kms:Decrypt` on its key when it is a customer managed key |
| logs and EMF metrics | `logs:CreateLogGroup`, `logs:CreateLogStream`, `logs:PutLogEvents` | the function's log group; EMF needs no `cloudwatch:PutMetricData` |
| `otel` with Active tracing | `xray:PutTraceSegments`, `xray:PutTelemetryRecords` | `*` (the `AWSXRayDaemonWriteAccess` managed policy) |
| `client`, `auth` | none | outbound network access to the host and the JWKS URL |

## Guide

[AWS configuration](https://docs.rs/davidrs/latest/davidrs/guide/aws_config/index.html), [DynamoDB](https://docs.rs/davidrs/latest/davidrs/guide/dynamodb/index.html), [EventBridge](https://docs.rs/davidrs/latest/davidrs/guide/eventbridge/index.html), [Secrets](https://docs.rs/davidrs/latest/davidrs/guide/secrets/index.html), [Outbound HTTP](https://docs.rs/davidrs/latest/davidrs/guide/outbound_http/index.html), [Bearer tokens](https://docs.rs/davidrs/latest/davidrs/guide/tokens/index.html), [Utilities](https://docs.rs/davidrs/latest/davidrs/guide/utilities/index.html), [Telemetry](https://docs.rs/davidrs/latest/davidrs/guide/telemetry/index.html), [Security on AWS](https://docs.rs/davidrs/latest/davidrs/guide/aws_security/index.html). Example: [`examples/table.rs`](https://github.com/eusoumaxi/davidrs/blob/main/examples/table.rs). The function template: [functions.md](functions.md); `Api` and `Access`: [http.md](http.md).
