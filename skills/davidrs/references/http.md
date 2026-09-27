HTTP endpoints with davidrs behind API Gateway (HTTP API, REST API) or a Lambda Function URL: the buffered `http::Api` pipeline and the streamed `StreamApi`, decoders, answers, failures and renderers, access control, quotas, validation, partial responses, and what API Gateway, AWS WAF, Amazon Cognito and CloudFront should do first.

# HTTP endpoints

One `Api` or `StreamApi` serves one operation: there is no router and no middleware stack. Application policy goes through the extension points (admission, policy, error renderer, finalizer, and `prepare` when streaming). The function around the endpoint (`App::from_env`, telemetry, deadlines, deployment) is in [functions.md](functions.md).

## Decide first

1. **Buffered or streamed.** `Api` (feature `http`) serves every request–response route: an HTTP API, a REST API (add `apigw-rest` for payload 1.0) or a `BUFFERED` Function URL. A response is at most 6 MB, and an HTTP API waits at most 30 s. `StreamApi` (feature `http-stream`) serves only a Function URL in `RESPONSE_STREAM` mode or a REST API method with the `STREAM` integration, for server-sent events and long or large responses; it has no admission stage, no decoder and no body-limit setting.
2. **Who proves the caller.** Behind API Gateway, the authorizer verifies the token (HTTP API JWT authorizer or REST API Cognito authorizer, with scopes on every route, since without them an ID token passes too) and `Access` reads its claims: the function does no token work. `Access::verify_bearer` (feature `auth`) with `.gateway_claims(false)` is for where no authorizer sits: a Function URL with auth `NONE`, a streamed route, a direct invocation. A hand-written `Policy` covers any other proof, such as an HMAC over `raw_body()`. `Public` admits everyone with the scope `()`.
3. **Error shape.** `PlainErrors` writes `{"errorCode", "errorMessage"}`; `ProblemErrors` (feature `problem`) writes RFC 9457 `application/problem+json`; a custom `ErrorRenderer` keeps an existing envelope.
4. **Counting.** WAF rate-based rules and API Gateway throttling stop floods before the function runs, and REST API usage plans meter partner keys. `RateLimited` (counter `DynamoWindow`, feature `dynamo`) is only for business quotas per verified user, tenant or operation.
5. **Validation.** Serde types give the shape; `validated_json` (feature `validate`) adds Garde rules and answers one generic `400`; `http::schema` lists every invalid field in one `400`.

## The buffered pipeline

`Api::new(operation, policy, renderer)` performs no I/O; `.body_limit(bytes)` (1 MiB by default), `.admission(..)` and `.finalize(|invocation, response| ..)` refine it; `.run(app, decode, handler)` serves the Lambda loop, and `.handle(app, request, &decode, &handler)` runs one request and returns the response, for tests. Every invocation runs, in this order:

1. **admission**, before anything is parsed: headers for the response, or a refusal;
2. **body limit, then the decoder** `Fn(&Request<'_>) -> Result<In, Failure>`: synchronous, no I/O; `|_| Ok(())` when there is no input;
3. **policy**: the `Scope`, or a `401` / `403`;
4. **handler** `async fn(Arc<App>, In, Context<Scope>) -> Result<Out, Failure>`, reading the caller from `context.scope()`;
5. **serialization** of `Out: IntoResponse`;

then the renderer for any failure (logged, with `logs`, as its operation, request id, code and kind, never its message) and the finalizer on every response. Steps 1 to 5 share the invocation deadline minus 100 ms; when it runs out the pending step is dropped and `504 ERROR_TIMEOUT` is rendered. Admission headers reach successes and failures alike.

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::http::{Api, Failure, HeaderValue, Json, PlainErrors, Public, Request, StatusCode};
use davidrs::{Context, RuntimeError};
use serde::Deserialize;

/// Built once in `main`, shared by every invocation.
struct App {
    table: String,
}

#[derive(Deserialize)]
struct OrderPath {
    id: String,
}

#[derive(Deserialize)]
struct NewNote {
    text: String,
}

/// Synchronous and bounded: the path parameter, then the JSON body.
fn decode(request: &Request<'_>) -> Result<(OrderPath, NewNote), Failure> {
    Ok((request.path()?, request.json()?))
}

/// `201` with the note's id, or `404 ERROR_NOT_FOUND` for an unknown order.
async fn add_note(
    app: Arc<App>,
    (path, note): (OrderPath, NewNote),
    context: Context<()>,
) -> Result<Option<(StatusCode, Json<String>)>, Failure> {
    if note.text.trim().is_empty() {
        return Err(Failure::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ERROR_EMPTY_NOTE",
            "The note has no text",
        ));
    }
    let id = context
        .deadline()
        .child(Duration::from_secs(2))
        .run(save_note(&app, &path.id, &note.text))
        .await?
        .map_err(|error| Failure::from_error("FAULT_STORE", &error))?;
    Ok(id.map(|id| (StatusCode::CREATED, Json(id))))
}

/// Stands in for the store call; `None` when the order does not exist.
async fn save_note(app: &App, order: &str, text: &str) -> Result<Option<String>, std::io::Error> {
    let _ = (&app.table, text);
    Ok(Some(format!("{order}-1")))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let app = Arc::new(App {
        table: davidrs::required_env("ORDERS_TABLE")?,
    });
    Api::new("add-order-note", Public, PlainErrors)
        .body_limit(4 * 1024)
        .finalize(|_invocation, response| {
            let no_store = HeaderValue::from_static("no-store");
            response.headers_mut().insert("cache-control", no_store);
        })
        .run(app, decode, add_note)
        .await
}
```

A request built by hand has no path parameters, gateway query pairs, `sourceIp` or authorizer claims. With `test-support` and `lambda_http = { version = "1", default-features = false, features = ["apigw_http"] }` in `[dev-dependencies]`, this is the body of a `#[tokio::test]` in `mod tests { use super::*; … }` next to the code above, with `std::collections::HashMap`, `davidrs::test_support` and `lambda_http::RequestExt as _` imported:

```rust
let api = Api::new("add-order-note", Public, PlainErrors).body_limit(4 * 1024);
let app = Arc::new(App {
    table: "orders".to_owned(),
});
let body = r#"{"text":" "}"#;
let request = test_support::post_json("https://example.com/orders/7/notes", body)
    .with_path_parameters(HashMap::from([("id".to_owned(), "7".to_owned())]));
let response = api.handle(app, request, &decode, &add_note).await;
assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
```

For query pairs, `sourceIp` or claims, parse a payload 2.0 event with `lambda_http::request::from_str(&json)`: `requestContext.authorizer.jwt.claims` holds strings, lists written `"[a b]"`, as an HTTP API JWT authorizer sends them. `test_support::get` and `post_json` attach a Lambda context; without one a request gets an empty request id and Lambda's maximum budget.

## Reading and answering

| `Request` read | Gives | Refuses with |
| --- | --- | --- |
| `path::<T>()` | the path parameters the gateway matched | `400 ERROR_INVALID_PATH` |
| `query::<T>()` | the raw query string | `400 ERROR_INVALID_QUERY` |
| `query_pairs()` | the gateway's pairs, repeats kept (`?tag=a&tag=b`) | — |
| `json::<T>()`, `json_bounded::<T>(bytes)`, `json_text()` | the body: typed, under a tighter limit, or as UTF-8 | `413 ERROR_BODY_TOO_LARGE`, `415 ERROR_UNSUPPORTED_MEDIA_TYPE`, `400 ERROR_MALFORMED_BODY` |
| `raw_body()` | the exact bytes, for a signature; the limit was checked first | — |
| `header(name)`, `method()`, `media_type()`, `native()` | one header, the method, `Content-Type` lowercased without parameters, the `lambda_http::Request` | — |
| `source_ip()` | the gateway's `sourceIp`, or `"unknown"`; `X-Forwarded-For` is ignored | — |

JSON without a `Content-Type` is accepted; a body that declares another media type is a `415` and is never parsed; an empty body is a `400`.

| The handler returns | The response |
| --- | --- |
| `Json(value)`, `NoContent` | `200` `application/json`; `204` without a body |
| `(StatusCode, T)`, `(StatusCode, HeaderMap, T)` | `T`'s response with that status, and those headers |
| `Option<T>` | `T`'s response, or `404 ERROR_NOT_FOUND` for `None` |
| `HttpResponse` | itself; `literal(status, media_type, body)` builds one that is not JSON |

A value that fails to serialize is a `500 FAULT_SERIALIZATION`, whatever status a tuple chose. This pipeline does no content negotiation or compression.

## Failures and renderers

- **4xx**: `Failure::new(status, code, message)`, with a message the client can act on. **5xx**: `Failure::internal(code, detail)`, `Failure::from_error(code, &error)` (the error's whole `source` chain becomes the detail) or `?` on a `RuntimeError` (`ERROR_TIMEOUT` for a deadline, `ERROR_LIMIT_EXCEEDED`, `FAULT_UNHANDLED`; all `500`). `public_message()` returns `InternalServerError` for every 5xx, and `Display` shows the same, so nothing a 5xx was built from reaches the client.
- `.with_detail(text)` is never rendered; `internal_detail()` reads it back for a deliberate log line. `.with_header(name, value)` and `.with_headers(pairs)` survive rendering (`Retry-After`, `Allow`); an invalid name or value is dropped.
- A catalog: `const ERRORS: &[ErrorDefinition] = &[ErrorDefinition::new(code, status, message), …];` and `ERRORS.failure(code)` (trait `ErrorCatalog`). An unknown code is `500 FAULT_UNHANDLED`, never a panic.
- The pipelines' own codes are constants in `davidrs::http::codes`: `INVALID_PATH`, `INVALID_QUERY`, `MALFORMED_BODY`, `INVALID_BODY`, `BODY_TOO_LARGE`, `UNSUPPORTED_MEDIA_TYPE`, `NOT_FOUND`, `TIMEOUT`, `SERIALIZATION`, `LIMIT_EXCEEDED`, `UNHANDLED`, the `Access` and `RateLimited` refusals below, and for `StreamApi` `MALFORMED_REQUEST`, `ORIGIN_NOT_ALLOWED`, `METHOD_NOT_ALLOWED`, `INVALID_ACCEPT`, `NOT_ACCEPTABLE`. A prefix is not a status: `ERROR_TIMEOUT` is a server failure.
- `ProblemErrors::default().with_type_base("https://example.com/errors/")` sets `type` to the base followed by the code (omitted without a base, meaning `about:blank`); `title` comes from the status and `detail` is the public message. It writes no extension members.

A custom renderer maps codes or keeps an envelope. It must not fail or panic, reads the message only through `public_message()`, and copies `headers()`; `literal` never fails (a media type that is not a valid header value gives an empty `500`), and headers every response needs belong in `finalize` instead:

```rust
use davidrs::http::{codes, literal, ErrorRenderer, Failure, HttpResponse};

/// `{"code", "message"}` with the failure's headers, so a `429` keeps its `Retry-After`.
struct SnakeErrors;

impl ErrorRenderer for SnakeErrors {
    fn render(&self, failure: &Failure) -> HttpResponse {
        let code = match failure.code() {
            codes::MALFORMED_BODY | codes::INVALID_BODY => "invalid_request",
            other => other,
        };
        let body = serde_json::json!({ "code": code, "message": failure.public_message() });
        let mut response = literal(failure.status(), "application/json", body.to_string());
        for (name, value) in failure.headers() {
            response.headers_mut().insert(name.clone(), value.clone());
        }
        response
    }
}
```

## Access and quotas

`Access` maps verified claims to the application's caller, requires what the route needs and hands the handler a `Grant` typed by it; `RateLimited` counts a business quota during admission. Behind an HTTP API JWT authorizer, with the tenant chosen per request:

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::aws::{sdk_config, Trust};
use davidrs::http::access::{Access, Claims, Grant, Tenancy};
use davidrs::http::rate_limit::DynamoWindow;
use davidrs::http::{Api, Failure, Json, PlainErrors, RateLimitConfig, RateLimited, Request};
use davidrs::{Context, RuntimeError};

/// The caller, built from claims the API Gateway authorizer verified.
struct User {
    id: String,
    tenants: Vec<String>,
    groups: Vec<String>,
}

fn user(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.subject()?.to_owned(),
        tenants: claims.list("tenants"),
        groups: claims.list("cognito:groups"),
    })
}

/// Admission runs before the policy: count what the authorizer verified.
fn verified_subject(request: &Request<'_>) -> String {
    Claims::from_gateway(request)
        .and_then(|claims| claims.subject().map(str::to_owned))
        .unwrap_or_else(|| "anonymous".to_owned())
}

async fn list_invoices(
    _app: Arc<()>,
    _input: (),
    context: Context<Grant<User, String>>,
) -> Result<Json<String>, Failure> {
    let grant = context.scope();
    Ok(Json(format!("{}/{}", grant.tenant(), grant.caller().id)))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let member = |user: &User, tenant: &str| user.tenants.iter().any(|own| own == tenant);
    let policy = Access::new(user)
        .require_caller()
        .tenancy(Tenancy::header("x-tenant-id", member))
        .require_tenant()
        .permit(|user, _tenant| {
            user.is_some_and(|user| user.groups.iter().any(|g| g == "billing"))
        });
    let quota = RateLimitConfig::new("invoices", 600, Duration::from_secs(3_600));
    let client = aws_sdk_dynamodb::Client::new(&sdk_config(Trust::NativeRoots)?);
    let counter = DynamoWindow::new(client, davidrs::required_env("QUOTA_TABLE")?);
    Api::new("list-invoices", policy, PlainErrors)
        .admission(RateLimited::new(counter, quota).key(verified_subject))
        .run(Arc::new(()), |_: &Request<'_>| Ok(()), list_invoices)
        .await
}
```

- **Grant types.** `Access::new(f)` gives `Grant<Option<C>, ()>`; `.require_caller()` makes the caller `C`; `.tenancy(Tenancy::header(name, member))` adds an `Option<String>` tenant and `.require_tenant()` makes it `String`. `.permit(|caller: Option<&C>, tenant: Option<&str>| ..)` runs last. `Tenancy::public()` lets an anonymous request name a tenant as context, never as membership; `Tenancy::or_else(f)` picks a tenant when the header is absent and is not checked against membership, so derive it from the caller's own tenants.
- **Order of refusals.** A token that is present but invalid: `401 ERROR_INVALID_TOKEN`. No caller where one is required: `401 ERROR_UNAUTHENTICATED`. A tenant the caller does not belong to, or an anonymous request naming one on a tenancy that is not public: `403 ERROR_FORBIDDEN`. No tenant where one is required: `400 ERROR_TENANT_REQUIRED`. `permit` says no: `403 ERROR_FORBIDDEN`. Other codes: `.refusals(Refusals { forbidden: ErrorDefinition::new(..), ..Refusals::default() })`.
- **Claims** read every authorizer shape: `subject()`, `string(name)`, `list(name)` (a JSON array, the HTTP API's `"[a b]"`, a space-separated `scope`), `contains(name, value)`, and `Claims::from_gateway(&request)` before a policy has run. REST API claims (a Cognito authorizer's `claims`, a Lambda authorizer's context) need `apigw-rest`. The gateway checks scopes, not groups: check `cognito:groups` in `permit`. `Access` loads nothing from a database; do such lookups in the handler, or write a `Policy`.
- **Trust.** Gateway claims come from the request context, which any principal allowed `lambda:InvokeFunction` can write. Grant `apigateway.amazonaws.com` the invoke permission with `AWS:SourceArn` naming the API, stage, method and path, and call `.gateway_claims(false)` wherever anything else can invoke the function. `verify_bearer` (feature `auth`) takes an `Arc<auth::Verifier>`, as in the streamed example below; the verifier is in [aws-services.md](aws-services.md).
- **Tokens the gateway verified.** Behind an authorizer, never verify the token again: map its claims. Claims arrive as strings (`"[a b]"` lists, which `Claims::list` reads); an object claim such as Keycloak's `realm_access` needs `.gateway_token_claims(true)`, which reads the verified token from the `Authorization` header only when its `iss` and shared text claims match the authorizer's. Typical mappings: Cognito `sub`, `cognito:groups`, `scope`; Auth0 `sub`, `permissions`, a namespaced roles claim; Entra ID `oid`, `roles`, `scp`; Okta `scp`; Clerk needs a JWT template that adds `aud`. An opaque (non-JWT) OAuth token needs a Lambda authorizer that introspects it and returns the caller in its context. Guide: https://docs.rs/davidrs/latest/davidrs/guide/access/index.html#tokens-the-gateway-already-verified
- **A hand-written `Policy`** sets `type Scope` (fields private, so only the policy builds one) and implements `async fn authorize(&self, request: &Request<'_>, invocation: &Invocation) -> Result<Self::Scope, Failure>`. Answer every token or signature failure with the same `401` and keep the reason in `with_detail`.
- **`RateLimited`** keeps a fixed window per key and refuses with `429 ERROR_RATE_LIMITED`, `Retry-After`, `RateLimit-Policy` and `RateLimit`; the budget headers go on successes too, and `.refusal(ErrorDefinition::new(..))` changes the refusal. `DynamoWindow::new(client, table)` writes one item per key and window under `PK` and `SK` with a TTL attribute `ttl` (or `.attributes(pk, sk, ttl)`): enable TTL on the table and grant `dynamodb:UpdateItem`. An unreachable counter is a `500`, never an uncounted request. Another store implements `Counter::hit(&self, key, config) -> Result<RateLimit, RuntimeError>`; a counter in memory only sees one instance's requests and is not a limit.
- **A hand-written `Admission`** (a maintenance switch, another limiter) implements `async fn check(&self, request: &Request<'_>, invocation: &Invocation) -> Result<Vec<(String, String)>, Failure>`: the headers it returns go on the response, and a refusal carries them with `with_headers`. `AdmitAll` is the default.
- **Keys** come from what the caller cannot choose: `Claims::from_gateway`, or `source_ip()` (the default) when no proxy sits in front. Behind CloudFront `sourceIp` is CloudFront's address; key on `CloudFront-Viewer-Address` (`ip:port`, forwarded by the origin request policy) only when the origin is reachable through the distribution alone. Never put a secret in a key.

## Validation and partial responses

With the `validate` feature, add `garde = { version = "0.23", default-features = false, features = ["derive"] }` (the version `davidrs` uses), derive `garde::Validate` next to `Deserialize`, put a rule on every field (`#[garde(length(chars, min = 1, max = 64))]`, `#[garde(range(min = 1, max = 100))]`, `#[garde(dive)]` for nested items, or `#[garde(skip)]`), and decode with `request.validated_json::<NewOrder>()`. A broken rule is `400 ERROR_INVALID_BODY` and the handler never runs. The failure does not say which field; only Garde's derive is enabled (no regex, email or URL rules); a rule that needs context runs in the handler with `validate_with`.

`http::schema` collects every problem into one `400` whose message lists `path: message` pairs separated by `; `. Decode a `serde_json::Value` with `request.json()`, create `Issues::new()`, read fields with `expect_object`, `expect_string`, `expect_string_min`, `expect_number(value, path, &NumberRule::int_between(1.0, 100.0), &mut issues)`, `expect_bool` and `expect_array`, add `check_unknown_keys` and `check_array_size`, then return `issues.into_result("ERROR_INVALID_ORDER")?` before deserializing the checked value. A wrong type or an unknown key aborts (`Issues::abort`); an out-of-range value is a check (`Issues::check`) and is still returned; gate a cross-field rule with `let mark = issues.mark();` and `issues.aborted_since(mark)`. A missing value `is required`, `null` is a wrong type, lengths count UTF-16 units, and the code is the service's.

A `fields` query (`fields=id,lines.price,-lines.price.tax`) is a `Mask`: `Mask::parse(fields.as_deref())` with the value found in `query_pairs()`. `mask.wants("customer")` skips a lookup; `mask.stored(FIELDS, &["id"])`, where `FIELDS` maps response fields to stored names, gives the attributes to read (`None`: all of them) for `DynamoProjection::new(attributes)` (its `expression()` and `names()` feed `projection_expression` and `expression_attribute_names`) or `sql_columns(&attributes)`; `mask.apply(&mut value)` trims the `serde_json::Value` before `Json(value)`; `mask.child("customer")` gives a nested builder its part. Arrays are transparent, unknown names keep nothing, paths stop at 32 levels, and there are no wildcards.

## Streaming

`StreamApi::new(operation, policy, renderer)` runs, in this order: read the payload (`400 ERROR_MALFORMED_REQUEST`); `prepare(|request| ..)`; CORS, where an `Origin` off the `Cors` list is `403 ERROR_ORIGIN_NOT_ALLOWED`, a request without `Origin` passes and `OPTIONS` is a `204` preflight; `methods(&[..])` (`GET` by default), else `405` with `Allow`; `Accept` negotiation (`stream::negotiate`), JSON unless `text/event-stream` has the higher quality (`400 ERROR_INVALID_ACCEPT`, `406 ERROR_NOT_ACCEPTABLE`); the policy; the handler. Negotiation, policy and handler run under the deadline minus `margin(..)` (one second by default), else `504`. Every response gets `Vary: Origin, Accept`, the `finalize(|invocation, headers| ..)` headers and, for an allowed origin, the CORS headers. A Function URL behind CloudFront origin access control, verifying the bearer token that a CloudFront Function moved out of `Authorization`:

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::auth::{Verifier, VerifierConfig};
use davidrs::client::{self, Limits};
use davidrs::http::access::{Access, Claims, Grant};
use davidrs::http::stream::{self, Cors, StreamApi, StreamRequest, StreamResponse};
use davidrs::http::{Failure, Method, PlainErrors, StatusCode};
use davidrs::{Context, RuntimeError};
use serde_json::json;

#[derive(serde::Deserialize)]
struct Query {
    text: String,
}

/// One JSON summary, or one event per batch for `Accept: text/event-stream`.
async fn search(
    _app: Arc<()>,
    request: StreamRequest,
    context: Context<Grant<String, ()>>,
) -> Result<StreamResponse, Failure> {
    let query: Query = request.view().json_bounded(4 * 1024)?;
    if !request.wants_events() {
        let summary = json!({ "query": query.text, "user": context.scope().caller() });
        return Ok(stream::json(StatusCode::OK, &summary));
    }
    let deadline = context.deadline().with_margin(Duration::from_secs(1));
    Ok(stream::events(deadline, move |events| async move {
        for batch in 1..=3 {
            let data = json!({ "query": query.text, "batch": batch }).to_string();
            let frame = stream::sse_frame(Some(&batch.to_string()), Some("results"), &data);
            if events.send(frame).await.is_err() {
                return;
            }
        }
    }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let issuer = davidrs::required_env("TOKEN_ISSUER")?;
    let tokens = VerifierConfig::new(issuer.clone(), format!("{issuer}/.well-known/jwks.json"))
        .with_audiences(davidrs::list_env("TOKEN_AUDIENCES")?)
        .with_required_claim("token_use", "access");
    let verifier = Verifier::load(client::build(Limits::default())?, tokens).await?;
    let policy = Access::new(|claims: &Claims| claims.subject().map(str::to_owned))
        .gateway_claims(false)
        .verify_bearer(Arc::new(verifier))
        .require_caller();
    let cors = Cors::new(davidrs::list_env("ALLOWED_ORIGINS")?)
        .allow_headers("accept,authorization,content-type,x-amz-content-sha256");
    StreamApi::new("search", policy, PlainErrors)
        .methods(&[Method::POST])
        .cors(cors)
        .prepare(|request| {
            if let Some(token) = request.headers_mut().remove("x-viewer-authorization") {
                request.headers_mut().insert("authorization", token);
            }
        })
        .run(Arc::new(()), search)
        .await
}
```

- The handler reads with `request.view()` (the bounded readers at the 1 MiB default: tighten with `json_bounded`, and call `check_body_limit()` before `raw_body()`), `native()`, `path()`, `event()` (the payload as delivered), `representation()` and `wants_events()`. It counts its own quota once it knows the caller: `counter.hit(&key, &config).await?`, then a `429` carrying `limit.headers()` when `limit.is_exceeded()`.
- Bodies: `stream::json(status, &value)` (a `204` has no body), `stream::events(deadline, producer)`, and `stream::from_response(response)` for a buffered `HttpResponse`. The response owns the producer: dropping it cancels the producer, the deadline bounds it, and it runs at most four frames ahead. `EventWriter::send` fails when the client is gone or a frame waited a second; the producer should stop then. `sse_frame(id, event, data)` writes one frame and drops line breaks from `id` and `event`; there are no keep-alive comments and no `Last-Event-ID` handling.
- `Cors::new(origins)` matches exact origins, no wildcards, and never sends `Access-Control-Allow-Credentials`: bearer tokens work, cookies do not. `.allow_headers(..)` defaults to `accept,authorization,content-type`; `.expose_headers(..)` and `.max_age(..)` (ten minutes) complete it. Leave the Function URL's own CORS configuration empty. Locally, start `cargo lambda watch --disable-cors`, or the emulator's permissive CORS headers hide the allowlist.
- `StreamApi::handle(app, event, &handler)` runs one invocation from a `LambdaEvent<serde_json::Value>`, for tests.

## The platform first

| Concern | API Gateway, AWS WAF, Cognito, CloudFront | The function |
| --- | --- | --- |
| Floods, abusive addresses, bots | WAF rate-based rules and managed rule groups; gateway throttling; a Function URL has only reserved concurrency | nothing |
| Token signature, expiry, scopes | JWT or Cognito authorizer, scopes on every route | `Access` reads the claims; `verify_bearer` only where no authorizer sits |
| Groups, tenants, rules over records | nothing | `permit`, `Tenancy`, the handler |
| Quotas | REST API usage plans for partner keys | `RateLimited` for business quotas |
| Body size | 10 MB at the gateway, 6 MB for Lambda; WAF inspects only the first 16 KB | `body_limit` per route |
| CORS | HTTP API or Function URL CORS configuration | `finalize` on a REST API proxy, `Cors` when streamed; one owner only |
| Timeouts | HTTP API 30 s, REST API 29 s by default | a function timeout below the gateway's, so the pipeline answers `504` |

A web ACL attaches to a REST API stage or a CloudFront distribution, never to an HTTP API or a Function URL: put CloudFront in front of those. A Function URL with auth `NONE` needs a resource policy granting `lambda:InvokeFunctionUrl` and `lambda:InvokeFunction` with the conditions `lambda:FunctionUrlAuthType` = `NONE` and `lambda:InvokedViaFunctionUrl` = `true`, and reserved concurrency as its only brake. Through origin access control the URL's auth type is `AWS_IAM`, a `POST` or `PUT` needs `x-amz-content-sha256` from the viewer, and a bearer token travels in another header.

## Pitfalls

- **Decoding runs before the policy.** An anonymous caller with a malformed body gets `400`, not `401`, and the decoder's work is spent before anyone is authorized: keep decoders cheap and free of I/O.
- **Admission cannot see the scope.** It runs before decoding and the policy: key quotas on verified identity, never on a header or parameter the caller writes.
- **Serialization cannot be interrupted**: the deadline does not preempt it. Bound response sizes with page limits.
- **A converted `RuntimeError` is a `500`**, an expired child deadline included; only the pipeline's own deadline renders `504`.
- **CORS has exactly one owner.** A Function URL with a CORS configuration adds its headers to the function's, and browsers reject the duplicates; an HTTP API with one replaces the function's.
- **The WAF inspection limit is not a body limit**: the rest of the body still reaches the function. Set `body_limit` per route.
- **`Api` has no `prepare`.** Behind origin access control, read a relocated token in a hand-written `Policy`, or use `StreamApi`.
- **Every 4xx message is public.** Identifiers of other users, internal names and secrets stay out of it; put diagnostics in `with_detail`.

## Guide

[HTTP](https://docs.rs/davidrs/latest/davidrs/guide/http/index.html), [access control](https://docs.rs/davidrs/latest/davidrs/guide/access/index.html), [security on AWS](https://docs.rs/davidrs/latest/davidrs/guide/aws_security/index.html), [tokens](https://docs.rs/davidrs/latest/davidrs/guide/tokens/index.html), [validation](https://docs.rs/davidrs/latest/davidrs/guide/validation/index.html), [partial responses](https://docs.rs/davidrs/latest/davidrs/guide/partial_responses/index.html), [streaming](https://docs.rs/davidrs/latest/davidrs/guide/streaming/index.html), [testing](https://docs.rs/davidrs/latest/davidrs/guide/testing/index.html). Runnable programs: `examples/http.rs`, `validated.rs`, `custom_policy.rs` and `stream_http.rs`.
