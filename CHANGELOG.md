# Changelog

Every notable change to `davidrs` is recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html): until 1.0, a minor
version may change the public API, and its entry says how.

## [Unreleased]

## [0.1.1](https://github.com/eusoumaxi/davidrs/compare/v0.1.0...v0.1.1) - 2026-09-29

### Changed

- The README now starts with installation, a complete HTTP function and
  direct links to the guide, examples, agent skill and security reporting.
- The introduction describes runtime boundaries without unmeasured latency
  or cost claims.
- The agent skill's references name `VerifierConfig::with_required_claim`
  and say that an `or_else` tenant is checked against membership.

### Fixed

- A token whose key is being fetched by a concurrent JWKS refresh waits for
  that refresh instead of being refused with `UnknownKey`.
- A `fields` exclusion outside what the request keeps no longer leaves an
  empty object in the response or marks its ancestors as wanted.
- A header name repeated on a `Failure` or by an `Admission`, such as
  `set-cookie`, keeps every value on the response.
- `sdk_config` reads the credentials again each time the SDK resolves them,
  so a value that changes while the function is warm is used.
- OpenAPI tools send a `null` field of a flattened JSON body as `null`
  instead of dropping it, so a JSON Merge Patch can clear a field.
- `Producer::fail` ends the stream: nothing sent afterwards, and no later
  deadline error, follows the failure, and the producer is cancelled.
- An MCP message with `"id": null` is refused with `-32600`, since MCP
  forbids a null request id; it was accepted as a notification and never
  answered.
- Keywords beside a `$ref` in an OpenAPI document apply together with its
  target in the published tool schemas: a constraint the target also sets
  goes under `allOf`, and `properties` and `required` are merged.
- `Invocation::invoked_arn` is `None` for a value that is not an ARN, such
  as the `function-arn` placeholder `cargo lambda watch` sends.
- An OpenAPI call sends one `Authorization` header: when an `authorization`
  header is configured with `OpenApi::header`, it replaces the caller's
  token that `OpenApi::forward_caller_token` would send.
- The debug log line of a refused (4xx) request records the step that
  refused it (`kind`) instead of the HTTP status, so two refusals with the
  same code from different steps can be told apart.
- A scheme name sent alone in `Authorization`, such as `Basic` or `NTLM`
  with no credentials, leaves the request anonymous on a route that
  verifies bearer tokens, instead of being refused with
  `ERROR_INVALID_TOKEN`.
- On an Application Load Balancer with multi-value headers, `source_ip`
  reads the last `X-Forwarded-For` value, the one the load balancer
  appends, instead of the first line, which a client can choose to pick
  its own rate-limit key.

## [0.1.0](https://github.com/eusoumaxi/davidrs/releases/tag/v0.1.0) - 2026-09-27

The first public release.

### Changed

- JWKS caches expire after one hour by default (`with_cache_ttl` configures
  this); expired keys require a successful refresh. Keys must include
  `kty: "RSA"`, and optional key usage fields must allow RS256 verification.
- Getting-started instructions, architecture boundaries, gateway trust
  requirements and agent guidance match the implementation. Performance
  claims without a reproducible benchmark have been removed.
- The guide, README and examples index now say when to read each chapter,
  what a caller or Lambda should observe when a call works, and what to
  change when it does not.

### Fixed

- Tenant fallbacks must pass the caller's membership check.
- OpenAPI path arguments cannot form `.` or `..` segments; mutating
  operations advertise potentially destructive effects to MCP clients.
- Unsupported critical JWT extensions are rejected. Failed or cancelled
  JWKS requests enter the refresh cooldown and redact URLs from errors.
- DynamoDB deadlines bound in-flight reads and batches. A zero item limit
  sends no query, and large deadline margins expire safely.
- Stream producers completing after their deadline report a timeout even
  when the runtime's timer has not fired yet.
- EMF serialization rejects non-finite metrics and field-name collisions
  that would overwrite CloudWatch metadata or invalidate the document.
- The table example scopes reads to the authenticated caller, reports
  incomplete pages and uses DynamoDB Local without an AWS account.

### Added

- Pipelines, one per trigger: buffered HTTP (`http::Api`), streamed HTTP with
  CORS and content negotiation (`http::stream::StreamApi`), SQS partial batches
  (`queue`), EventBridge rules (`event`), schedules (`schedule`), direct
  invocations (`runtime`) and response streams that own their producer
  (`streaming`).
- One absolute invocation deadline (`Deadline`) shared by every stage, with a
  margin kept before Lambda's own timeout.
- Failures that keep their public message and internal detail apart;
  the built-in `PlainErrors` and RFC 9457 `ProblemErrors` renderers use
  a fixed message for every 5xx.
- Extension points: `Policy`, `Admission`, `ErrorRenderer`, response
  finalizers and the streamed pipeline's `prepare` hook.
- Access control by configuration: `http::access::Access` identifies callers
  from API Gateway authorizer claims or a verified bearer token, requires them
  or not, selects tenants, applies permission rules and refuses with
  configurable codes; handlers receive a `Grant` typed by those guarantees.
  Behind an API Gateway authorizer, `Claims::from_gateway_token` and
  `Access::gateway_token_claims` read the verified token's claims with their
  JSON types, without verifying it again.
- Rate limiting: the `RateLimited` admission with a pluggable `Counter`, a
  DynamoDB fixed-window counter, and the IETF `RateLimit-Policy` and
  `RateLimit` header fields.
- Partial responses pushed down to the data layer: `fields` masks,
  `Mask::stored`, `DynamoProjection` and `sql_columns`.
- Request validation that reports every invalid field at once (`http::schema`),
  and Garde validation (`validate`).
- MCP servers on Lambda (`mcp`, `mcp-openapi`): tools written by hand or
  generated from an OpenAPI document, with OAuth protected-resource metadata.
- AWS helpers: SDK configuration from the Lambda environment; bounded DynamoDB
  reads, batches and signed page tokens; EventBridge publishing with per-entry
  outcomes; typed Secrets Manager reads that never echo a value.
- Outbound HTTP with byte and time limits, RS256 / JWKS bearer-token
  verification, bounded gzip, SHA-256 and in-process caches.
- Telemetry: logs, CloudWatch EMF metrics and X-Ray traces through the
  sandbox's agent.
- Test support: synthetic invocations and requests for testing handlers.
- `Request::form` for `application/x-www-form-urlencoded` bodies, and the
  `alb` feature for Application Load Balancer events.
- Handler errors of the non-HTTP triggers convert into `lambda_runtime`'s
  `Diagnostic` (re-exported as `runtime::Diagnostic`), so a function chooses
  the `errorType` a Step Functions `Retry` or `Catch` matches; a
  `RuntimeError` reports its variant.
- Logs follow `AWS_LAMBDA_LOG_LEVEL` when `RUST_LOG` is unset, and
  `telemetry::logs::runtime_span_filter` drops the runtime's own invocation
  span.
- Streamed responses carry `Set-Cookie` headers in the stream's cookie list.
- Rust 2024 edition; the minimum supported Rust is 1.94.1, checked in CI.
