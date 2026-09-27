# Architecture

`davidrs` is one crate: value types at the root, and one module per capability, each behind the feature of the same name. This chapter follows an invocation through the crate: how it becomes an [`Invocation`](crate::Invocation) with a deadline, how each pipeline runs, how failures are rendered, and where an application plugs in.

## Layout

```text
src/
  lib.rs           the index and the feature table
  context.rs       Invocation, Deadline, Context          always
  env.rs           required_env, optional_env, list_env   always
  error.rs         RuntimeError, error_chain              always
  runtime.rs       the native loop, invocation metadata   runtime
  http.rs          the buffered HTTP pipeline             http
    http/api.rs        Api: admission → decode → policy → handler → response
    http/request.rs    bounded, typed reads of a request
    http/response.rs   IntoResponse, Json, NoContent
    http/failure.rs    Failure, FailureKind, error catalogs
    http/render.rs     ErrorRenderer, PlainErrors, ProblemErrors
    http/policy.rs     Policy, Admission, Public, AdmitAll
    http/codes.rs      the codes the pipelines raise themselves
    http/access.rs     Access: callers, tenants, permissions
    http/rate_limit.rs RateLimited, counters, the RateLimit headers
    http/fields.rs     partial responses (fields=a,b.c,-d)
    http/schema.rs     collecting every invalid field
    http/stream.rs     StreamApi: the streamed pipeline       http-stream
  streaming.rs     StreamBody, Producer                   streaming
  queue.rs         SQS partial batches, Visibility        queue
  event.rs         typed EventBridge input                event
  schedule.rs      typed scheduled input                  schedule
  aws.rs           SDK configuration                      aws
  table.rs         DynamoDB outcomes, bounds, page tokens dynamo
  events.rs        EventBridge publishing                 events
  secrets.rs       Secrets Manager reads                  secrets
  client.rs        outbound HTTP with limits              client
  auth.rs          RS256 / JWKS verification              auth
  cache.rs         in-process caches                      cache
  compression.rs   bounded gzip                           compression
  digest.rs        SHA-256, hex                           digest
  telemetry.rs     logs, EMF metrics, X-Ray traces        logs, metrics, otel
  mcp.rs           MCP servers: protocol, tools, OpenAPI  mcp, mcp-openapi
  test_support.rs  synthetic invocations and requests     test-support
```

## The invocation

Every adapter turns Lambda's context into an [`Invocation`](crate::Invocation): the request id, the raw `X-Amzn-Trace-Id`, the invoked function ARN, the moment it arrived, and a [`Deadline`](crate::Deadline).

Lambda reports its deadline as epoch milliseconds. It is converted to a monotonic instant immediately, so a wall-clock step cannot move the budget afterwards. A local emulator that sends a relative budget instead (Cargo Lambda sends `600000`) is read as "that long from now": no real epoch value is that small.

Each adapter keeps a reserve before Lambda's own timeout: 100 ms for buffered responses and queue records, one second by default for a streamed response, which still has to flush its last frames. Work runs under `deadline.with_margin(reserve).run(work)`; when the budget runs out the future is dropped at its next await. Anything that must survive cancellation — releasing a lease, saving state — belongs outside the bounded future, with its own [`child`](crate::Deadline::child) budget.

## The buffered HTTP pipeline

[`Api`](crate::http::Api) serves API Gateway HTTP APIs, REST APIs (with `apigw-rest`) and Function URLs with one buffered response:

```text
lambda_http::Request
  │ invocation: request id, trace id, deadline
  ├─ 1. Admission::check      async, before parsing   → headers, or a Failure
  ├─ 2. body limit + decode   sync, bounded           → Input, or 400 / 413 / 415
  ├─ 3. Policy::authorize     async                   → Scope, or 401 / 403
  ├─ 4. handler(app, Input, Context<Scope>)           → Output: IntoResponse
  ├─ 5. Output::into_response sync, fallible          → response, or 500
  │     steps 1–5 share deadline − 100 ms; running out is a 504
  ├─ a failure? → log safe metadata → ErrorRenderer::render
  └─ finalizer(invocation, &mut response)             success and failure alike
```

- **Admission runs first**, so a rate limit refuses a request whose body is garbage without parsing it, and its headers are attached to whatever response follows — success or refusal.
- **Decoding is synchronous**: the body is already in memory. Every decoder shares one body limit (1 MiB by default).
- **Serialization is fallible**: a success value that cannot be serialized is rendered as a 500, never as a `200` with a broken body.
- **One exit**: the finalizer sees every response, which is where headers that belong on everything (cache control, a request id) are added.

## The streamed HTTP pipeline

[`StreamApi`](crate::http::stream::StreamApi) answers through a Lambda response stream (a Function URL in `RESPONSE_STREAM` mode, or a REST API method with the `STREAM` integration):

```text
LambdaEvent<Value>
  │ parse into lambda_http::Request             malformed → 400
  ├─ prepare(&mut request)                      optional request rewrite
  ├─ CORS: Origin outside the allowlist         → 403
  ├─ OPTIONS                                    → 204 preflight
  ├─ method not served                          → 405 with Allow
  ├─ negotiate(Accept)                          JSON or text/event-stream, else 400 / 406
  ├─ Policy::authorize                          → Scope
  ├─ handler(app, StreamRequest, Context<Scope>) → StreamResponse
  │     negotiate..handler share deadline − margin (1 s); running out is a 504
  ├─ a failure? → log safe metadata → ErrorRenderer::render → streamed
  └─ Vary: Origin, Accept · finalizer · CORS headers for an allowed origin
```

Decoding and admission are the handler's here: a streamed endpoint often decides what to count only once it knows who is calling, which a stage before the policy could not express. A response is either one JSON document or an event stream whose producer the response body owns.

## The other triggers

| Trigger | Entry point | Handler input | Handler output |
| --- | --- | --- | --- |
| SQS | [`queue::run`](crate::queue::run) | one [`Delivery`](crate::queue::Delivery) at a time | [`Disposition`](crate::queue::Disposition) |
| EventBridge | [`event::run`](crate::event::run) | [`Event<T>`](crate::event::Event) | `()` |
| Schedule | [`schedule::run`](crate::schedule::run) | the scheduled payload `T` | `()` |
| Direct invocation | [`runtime::run`](crate::runtime::run) | any payload `T` | any response `U` |
| Streamed, any payload | [`streaming::run`](crate::streaming::run) | any payload `T` | a head and a [`StreamBody`](crate::streaming::StreamBody) |

All of them run the handler under the invocation deadline and open one tracing span per invocation.

## Failures

A [`Failure`](crate::http::Failure) is the only thing an HTTP handler returns on the error path:

| Part | Rendered? | Purpose |
| --- | --- | --- |
| status | yes | the HTTP status |
| code | yes | a stable, machine-readable identifier |
| message | 4xx only | what the client may read |
| detail | never | what a deliberate diagnostic may log |
| kind | never | the stage that failed: admission, decode, policy, handler, serialization, deadline |
| headers | yes | `Retry-After`, `RateLimit`, `Allow` survive rendering |

[`public_message`](crate::http::Failure::public_message) returns the message of a 4xx and a fixed `InternalServerError` for every 5xx, however the failure was built. Renderers can only read the message through it, which makes a leaking 5xx impossible rather than unlikely. Default logs record the operation, the request id, the code and the kind — never the detail.

The pipelines raise their own failures under the codes in [`http::codes`](crate::http::codes). An application whose clients expect other codes maps them in its renderer, which sees the whole failure and owns the wire format.

## Extension points

| Trait or hook | Decides | Default |
| --- | --- | --- |
| [`Policy`](crate::http::Policy) | who is calling, and the scope the handler runs with | [`Public`](crate::http::Public), scope `()` |
| [`Admission`](crate::http::Admission) | whether a buffered request may be parsed at all | [`AdmitAll`](crate::http::AdmitAll) |
| [`ErrorRenderer`](crate::http::ErrorRenderer) | the wire shape of a failure | [`PlainErrors`](crate::http::PlainErrors) `{errorCode, errorMessage}`, or [`ProblemErrors`](crate::http::ProblemErrors) (RFC 9457) |
| `finalize` on either pipeline | headers every response carries | none |
| [`StreamApi::prepare`](crate::http::stream::StreamApi::prepare) | request rewrites before anything reads it | none |
| [`Cors`](crate::http::stream::Cors) | allowed origins, allowed and exposed headers, preflight age | none |

A policy's `Scope` is evidence: it exists only because the policy succeeded. Keep its fields private so nothing downstream can build one by hand.

Clients, configuration, repositories and payload types stay concrete in the application: applications do not differ on them in a way a trait would capture.

## Streaming ownership

[`StreamBody::spawn`](crate::streaming::StreamBody::spawn) starts the producer as a task and keeps its handle and a cancellation token. Dropping the body — the client went away — cancels the token, gives a cooperative producer 50 ms, then drops its future. The producer's own deadline bounds it even while a client keeps reading. A producer panic arrives as a final error item instead of a stream that merely looks complete.

## AWS helpers

[`aws::sdk_config`](crate::aws::sdk_config) builds the SDK configuration from the credentials and region Lambda injects, over rustls with a caller-chosen root bundle. The default provider chain (profiles, IMDS, STS) never runs inside Lambda and is not compiled in.

The per-service modules own partial outcomes and bounds, and nothing else. Keys, conditions and item shapes stay in the application, written with the SDK's builders: there is no repository abstraction and no mapper, because they would hide exactly what matters — which items were read or accepted.

## Concurrency

Shared state is immutable behind `Arc`. Independent I/O uses `join` or bounded task sets. No lock is held across an await, and the JWKS cache coalesces refreshes behind an async mutex with a minimum interval, so a flood of unknown key ids causes one fetch. A timeout cancels local waiting; it cannot undo a remote write, so use idempotency where a write can complete after the connection is lost.
