# Security

## Supported versions

Security fixes go to the latest release. Before 1.0, that is the latest `0.x` minor version: a fix for `0.3` ships as a `0.3.y` patch, and older minor versions are not patched.

## Reporting a vulnerability

Report vulnerabilities privately, never in a public issue or pull request. A report we can act on names the version and the Cargo features, says what an attacker can do that they should not be able to do, and includes the smallest code or request that you have already run. A scanner finding or a model suggestion that you have not reproduced is closed without review. Do not include real credentials, tokens or customer data in the report or in a test fixture.

Send it:

- through GitHub's [private vulnerability reporting](https://github.com/eusoumaxi/davidrs/security/advisories/new), or
- by email to <hi@eusoumaxi.com>.

## What happens next

You get an acknowledgement within 7 days. The fix is developed in a private advisory, released as a patch version, and then disclosed: the GitHub security advisory is published, the vulnerability is submitted to the [RustSec advisory database](https://rustsec.org) so `cargo audit` and `cargo deny` warn every user, and the report is credited unless you prefer otherwise.

## Threat model

The recommended deployment puts CloudFront, AWS WAF, API Gateway and Amazon Cognito in front of the function, so floods, bad tokens and abusive addresses are refused before it runs (the guide's security chapter explains the split). The guarantees below hold with or without them.

The crate assumes four hostile parties: an anonymous internet client reaching a function through API Gateway, a Function URL or CloudFront; an authenticated user acting for a tenant that is not theirs; an upstream service answering an outbound call with a hostile status, body or size; and an MCP client, or a model choosing tool arguments, driving a server. A principal allowed to invoke the function directly is trusted as far as IAM trusts it, which is why the gateway-claims note below matters.

## What the crate guarantees

Each of these is enforced in code and covered by a test in `tests/`.

- **No leaking 5xx.** `Failure::public_message` is the only accessor a renderer has for the message, and it returns a fixed string for every 5xx; the internal detail is never rendered. The built-in renderers (`PlainErrors`, `ProblemErrors`, the MCP JSON-RPC errors and tool results) read it through that accessor, and a custom `ErrorRenderer` must do the same. The pipelines log a failure's operation, request id, code and kind, never its message or detail.
- **Safe decoding errors.** Serde and Garde decoding failures render fixed messages. Only application validation (`http::schema`) and an MCP tool's argument errors echo field names and values, as JSON, to the caller who sent them.
- **Bounded input.** Request bodies (1 MiB by default, checked before any decoder runs, including one that reads the raw bytes), MCP request bodies, upstream responses read through `client::read_bounded`, JWKS documents (64 KiB, 32 keys), tokens (8 KiB), decompressed payloads and `fields` masks (32 levels) all have limits checked before parsing. Paginated reads and batches stop at explicit caps and say so.
- **Pinned token algorithm and audience.** `auth` accepts RS256 only, requires `kid`, `iss` and `exp`, refuses every token until the accepted audiences are configured (matching `aud`, or `client_id` when a token has none, as API Gateway does; `without_audience_check` is an explicit opt-out), honours `nbf`, compares times as floating point so an absurd value cannot wrap into the opposite verdict, and lets an unknown key id cause at most one JWKS fetch per refresh interval (five seconds after a failed one), shared by every concurrent request.
- **Headers cannot be injected.** Every response header the crate builds from a value goes through `HeaderValue::try_from`, and an invalid one is dropped rather than written. `sse_frame` removes line breaks from `id` and `event`.
- **Browser origins are an allowlist.** `Cors` and `Server::allow_origins` compare `Origin` exactly with configured values and echo nothing else; an MCP server allows no browser origin by default.
- **Rate limits fail closed.** `RateLimited` refuses with a 500 when its counter cannot be reached; no request is admitted uncounted.
- **Outbound calls are bounded and stay where they were sent.** `client::build` uses rustls with the Mozilla roots compiled in, TLS 1.2 and 1.3, connect and request timeouts and no redirects, and `send_error` keeps URLs out of errors. An OpenAPI tool call goes to the configured base URL followed by the document's path, with path and query values percent-encoded; a document whose path does not start with `/` is refused, and no tool argument can set `Host`, `Content-Length`, `Transfer-Encoding`, `Accept`, `Content-Type` or `Authorization`.
- **No secrets in errors.** `secrets` errors name the secret, never its value, and a malformed JSON secret reports a line and column only; `env` errors name the variable; `client` errors carry no URL.
- **Tenants are membership.** `Access` refuses a tenant the caller does not belong to, and refuses an anonymous request that names a tenant unless the tenancy is explicitly `public`.
- **Tokens stay with the service they were issued for.** An MCP OpenAPI tool forwards the caller's `Authorization` header only when `forward_caller_token` is set, and only to the configured base URL's host; a fixed credential set with `header` is marked sensitive and never appears in `Debug` output.
- **Page tokens can be signed.** `encode_cursor_signed` binds a page token to an HMAC-SHA256 under a server secret; `decode_cursor_signed` verifies it in constant time and treats an edited or forged token as "first page".
- **Test support has no bypass.** The `test-support` feature builds synthetic invocations and requests only. Nothing in the crate can construct a `Grant` or `VerifiedClaims` without the checks that produce them.
- **No `unsafe`.** The library is `#![forbid(unsafe_code)]`, which no attribute can lift. The only unsafe code in the repository is the tests' helper that writes environment variables, under a lock. Dependencies are checked with `cargo deny` for advisories, licences and sources.

## What it leaves to the application

- **Authorization.** `Policy` decides who may do what. `Access` verifies tokens, requires a caller and checks tenant membership; it loads no permissions. Keep a scope's fields private so nothing downstream can fabricate one.
- **Volume and reputation.** Floods, bots and abusive addresses are the platform's job: AWS WAF rate-based rules and managed rule groups, and API Gateway throttling, refuse them before the function is invoked. `RateLimited` enforces business quotas — per user, tenant or key — on requests that were already admitted, and fails closed.
- **The audience of a token.** Set `with_audiences` to the resource each verifier protects — for an MCP server, its own resource URI — and reserve `without_audience_check` for an issuer that serves this application alone.
- **Gateway claims are trusted.** `Access` reads an API Gateway authorizer's claims from the request context by default. Any principal allowed to invoke the function directly can write that context, so grant `lambda:InvokeFunction` only to `apigateway.amazonaws.com` with an `AWS:SourceArn` naming the API, stage, method and path, or turn gateway claims off and verify tokens in the function.
- **A public tenancy.** With `Tenancy::public`, an anonymous request may name any tenant as context. Treat that tenant as public context — a storefront, a catalogue — never as membership.
- **Decoding runs before the policy.** In the buffered pipeline the body is decoded before `Policy::authorize`, so an anonymous caller can exercise the (bounded) decoder and learn a 400 before a 401. Put expensive or secret-dependent validation in the handler.
- **Tokens that cross services.** Turn on `forward_caller_token` only when the API is your own and accepts tokens issued for the MCP server's resource, as the guide's chapter on tokens that cross services explains. With a `Public` policy, an OpenAPI server is an open proxy to the selected operations. Build the upstream client with `client::build` so redirects stay off.
- **Page tokens.** Prefer the signed pair for tokens a client holds; an unsigned cursor from `encode_cursor` can be read and edited. Either way, keep the tenant in the query's key condition, never only in the token: a signed token is tamper-proof, not secret.
- **Outbound URLs.** `client` has no host allowlist and no HTTPS-only mode. Build URLs from configuration, never from request input.
- **Logs and error records.** The crate logs safe metadata only, but a handler's own error text reaches Lambda's error record on direct, queue, event and schedule invocations, `Failure`'s `Debug` output includes its internal detail, and span attributes an application sets are exported as they are.
- **Deadlines cancel waiting, not remote writes.** Use idempotency keys where a write can complete after the connection is lost. Synchronous decoding and serialization cannot be preempted, so keep payload types bounded.
- **Rate-limit keys.** The default key is the gateway's `sourceIp`; a request without one counts as `"unknown"`, one bucket shared by every such caller. A custom key is part of the stored item's identity: keep it short and never put a secret in it.
