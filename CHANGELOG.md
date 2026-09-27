# Changelog

All notable changes are recorded here. The project follows
[Semantic Versioning](https://semver.org); until 1.0, a minor version may
change the public API and says how.

## 0.1.0 — unreleased

The first public release.

- Pipelines: buffered HTTP (`http::Api`), streamed HTTP with CORS and content
  negotiation (`http::stream::StreamApi`), SQS partial batches (`queue`),
  EventBridge (`event`), schedules (`schedule`), direct invocations
  (`runtime`) and owned response streams (`streaming`).
- Invocation deadlines (`Deadline`) shared by every stage, with a cleanup
  margin before Lambda's timeout.
- Failures with separate public and internal text; every 5xx renders a fixed
  message. `PlainErrors` and RFC 9457 `ProblemErrors` renderers.
- Extension points: `Policy`, `Admission`, `ErrorRenderer`, response
  finalizers and a request `prepare` hook.
- Access control by configuration: `http::access::Access` identifies callers
  from API Gateway authorizer claims or a verified bearer token, requires them
  or not, selects tenants, applies permission rules and refuses with
  configurable codes; handlers receive a `Grant` typed by those guarantees.
- Rate limiting: the `RateLimited` admission with a pluggable `Counter`, a
  DynamoDB fixed-window counter, and the current IETF draft headers
  (`RateLimit-Policy`, `RateLimit`).
- Partial responses pushed down to the data layer: `Mask::stored`,
  `DynamoProjection` and `sql_columns`.
- MCP servers on Lambda (`mcp`, `mcp-openapi`): tools declared by hand or
  generated from an OpenAPI document, with OAuth protected-resource metadata.
- AWS helpers: SDK configuration from the Lambda environment, bounded
  DynamoDB reads and batches with page tokens, EventBridge publishing and
  Secrets Manager reads with per-entry outcomes and no echoed values.
- Outbound HTTP with byte and time limits, RS256 / JWKS token verification,
  bounded gzip, SHA-256, in-process caches.
- Telemetry: logs, CloudWatch EMF metrics and X-Ray traces through the
  sandbox agent.
