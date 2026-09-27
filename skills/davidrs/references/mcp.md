Serving MCP tools to AI clients from one AWS Lambda function with davidrs: hand-written tools, tools from an OpenAPI document, API keys and OAuth, visibility and rate limits, deployment, testing and connecting a client.

# MCP servers

`mcp::Server` (feature `mcp`) is a Model Context Protocol server in one Lambda function. It speaks revision 2026-07-28 (`mcp::PROTOCOL_VERSION`) over Streamable HTTP, answers each request with one JSON object and keeps no session, so any instance serves any request. It runs on the buffered HTTP pipeline: an HTTP `Policy` authenticates every `POST`, and its `401` points OAuth clients at the protected-resource metadata. With `mcp-openapi`, `mcp::openapi::OpenApi` turns the operations of an OpenAPI document into tools.

## Features and manifest

| Need | Feature |
| --- | --- |
| hand-written tools | `mcp` (implies `http`) |
| tools from an OpenAPI document | `mcp-openapi` (implies `mcp` and `client`) |
| tokens verified in the function, `auth::bearer` | `auth` |
| API keys stored as SHA-256 hashes | `digest` |
| `RateLimited` counting in `DynamoWindow` | `dynamo` |
| logs, including each failed tool's code | `logs` |
| tests through `Server::handle` | `test-support`, in `[dev-dependencies]` only |

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["mcp", "auth", "digest", "logs"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

[dev-dependencies]
davidrs = { version = "0.1", default-features = false, features = ["test-support"] }
```

## Rules

- One function is one server: build it once in `main` with `Server::new(name, version, policy)`, add the tools, then `run(Arc::new(app))`. Clients, tables and secrets live in `App`, built in `main`.
- A tool handler has the HTTP handler shape, `async fn(Arc<App>, Args, Context<Scope>) -> Result<T, Failure>`, where `Args: Deserialize`, `T: Serialize` and `Scope` is what the policy establishes: `()` for the API-key policy below, `Grant<String, ()>` for `Access::new(user).require_caller()`.
- The input schema is what the model writes arguments from. The server neither generates nor validates it: describe every property and keep it in step with `Args`.
- Refusals, bad arguments and upstream failures are tool results with `isError: true`, which the model reads and corrects; only malformed requests are JSON-RPC errors. Refuse with a 4xx `Failure` whose message helps the model. A 5xx shows the fixed `InternalServerError`: its detail is never sent, and the server logs only the code and kind.
- Authenticate every caller, and accept only tokens issued for this server's resource URI. Behind a Function URL with auth type `NONE`, the policy is the only gate.
- `Server::visible` hides tools per caller, and the handler still checks: the list and annotations such as `readOnlyHint` are hints, not permissions.
- Never forward a caller's token to an API it was not issued for. Select tools deliberately: every definition costs the model context on every turn, and a server holds at most `MAX_TOOLS` (128).

## A complete server with API keys

The quickest start, and the right credential for automation: a long-lived key per caller, of which only the SHA-256 is stored (features `mcp`, `auth`, `digest`, `logs`):

```rust
use std::collections::HashSet;
use std::sync::Arc;

use davidrs::auth::bearer;
use davidrs::digest::sha256_hex;
use davidrs::http::{codes, Failure, Policy, Request, StatusCode};
use davidrs::mcp::{Server, Tool};
use davidrs::{list_env, Context, Invocation, RuntimeError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Everything the tools reach, built once in `main`.
struct App {
    prices: Vec<(String, u32)>,
}

/// The SHA-256 of every API key allowed to call. The keys themselves are
/// stored nowhere.
struct ApiKeys {
    hashes: HashSet<String>,
}

impl Policy for ApiKeys {
    type Scope = ();

    async fn authorize(&self, request: &Request<'_>, _: &Invocation) -> Result<(), Failure> {
        match request.header("authorization").map(bearer) {
            Some(key) if self.hashes.contains(&sha256_hex(key.as_bytes())) => Ok(()),
            _ => Err(Failure::new(
                StatusCode::UNAUTHORIZED,
                codes::UNAUTHENTICATED,
                "A valid API key is required",
            )),
        }
    }
}

/// The arguments of `get_price`, exactly as `price_schema` describes them.
#[derive(Deserialize)]
struct PriceQuery {
    item: String,
}

#[derive(Serialize)]
struct Price {
    item: String,
    cents: u32,
}

fn price_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "item": { "type": "string", "description": "The item's name, such as \"pen\"" }
        },
        "required": ["item"],
        "additionalProperties": false
    })
}

/// An unknown item is a refusal the model can read and correct.
async fn get_price(app: Arc<App>, query: PriceQuery, _: Context<()>) -> Result<Price, Failure> {
    app.prices
        .iter()
        .find(|(item, _)| *item == query.item)
        .map(|(item, cents)| Price { item: item.clone(), cents: *cents })
        .ok_or_else(|| Failure::new(StatusCode::NOT_FOUND, "ERROR_UNKNOWN_ITEM", "No item with this name"))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("catalogue-mcp")?;
    let keys = ApiKeys { hashes: list_env("API_KEY_HASHES")?.into_iter().collect() };
    let app = Arc::new(App { prices: vec![("pen".to_owned(), 150)] });
    Server::new("catalogue", "1.0.0", keys)
        .instructions("Prices of the catalogue's items, in cents.")
        .tool(
            Tool::new("get_price", "The price of one item, in cents", price_schema(), get_price)
                .annotations(json!({ "readOnlyHint": true })),
        )
        .run(app)
        .await
}
```

What the server does with a tool:

- **Name and description**: the name is 1 to 128 ASCII letters, digits, `_`, `-` or `.`, unique; a bad name, a duplicate or a 129th tool panics when the server is built, on the first cold start. The description is what the model reads to choose the tool: say what it does and what it returns.
- **Arguments** that do not deserialize into `Args` are refused with `INVALID_ARGUMENTS` (`ERROR_INVALID_ARGUMENTS`) before the handler runs. A tool without arguments takes `serde_json::Value` and the schema `{"type": "object", "additionalProperties": false}`.
- **A success** that serializes to a JSON object is returned as `structuredContent` and as text; a string is returned as text.
- **A `Failure`** becomes `isError: true` with the text `CODE: public message`.
- **Time**: the handler runs under the invocation deadline less `CALL_MARGIN` (1 s), and `context.deadline()` already reflects it, so child budgets leave time for the answer. A handler still running at the deadline is dropped and answered with `ERROR_TIMEOUT` under its JSON-RPC id.
- **`instructions`** is guidance a client may add to the model's context; **`annotations`** are untrusted hints, such as a client skipping a confirmation for `readOnlyHint`.

## OAuth: tokens issued for this server

Replace the policy with `Access` verifying bearer tokens against your issuer, with the server's resource URI as the audience, and publish the resource metadata so OAuth clients find where to sign in (features `mcp`, `auth`, `logs`). Tool handlers then receive `Context<Grant<String, ()>>`:

```rust
use std::sync::Arc;

use davidrs::auth::{Verifier, VerifierConfig};
use davidrs::client::{self, Limits};
use davidrs::http::access::{Access, Claims, Grant};
use davidrs::http::Failure;
use davidrs::mcp::{ProtectedResource, Server, Tool};
use davidrs::{required_env, Context, RuntimeError};
use serde_json::{json, Value};

/// The caller: the subject of a token issued for this server.
fn user(claims: &Claims) -> Option<String> {
    claims.subject().map(str::to_owned)
}

async fn whoami(_: Arc<()>, _: Value, context: Context<Grant<String, ()>>) -> Result<String, Failure> {
    Ok(format!("You are {}", context.scope().caller()))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders-mcp")?;
    let issuer = required_env("COGNITO_ISSUER")?;
    let resource = required_env("MCP_URL")?;
    let tokens = VerifierConfig::new(issuer.clone(), format!("{issuer}/.well-known/jwks.json"))
        .with_audiences(vec![resource.clone()])
        .with_required_claim("token_use", "access");
    let verifier = Arc::new(Verifier::load(client::build(Limits::default())?, tokens).await?);
    let policy = Access::new(user).gateway_claims(false).verify_bearer(verifier).require_caller();
    let no_arguments = json!({ "type": "object", "additionalProperties": false });
    Server::new("orders", "1.0.0", policy)
        .protected_resource(ProtectedResource::new(resource, [issuer]))
        .tool(Tool::new("whoami", "Says who the caller is", no_arguments, whoami))
        .run(Arc::new(()))
        .await
}
```

`gateway_claims(false)` because nothing in front of a Function URL writes trustworthy claims; keep the default behind an API Gateway JWT authorizer. `ProtectedResource::new(resource, authorization_servers)` takes the canonical URI clients are configured with; `.scopes([..])` publishes `scopes_supported` and adds `scope="…"` to the challenge.

How an OAuth client signs in (OAuth 2.1, discovered from the server):

1. A call without a token gets `401` with `WWW-Authenticate: Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource"`. With `protected_resource` set, the server adds this to every `401` its policy returns, an API key's included.
2. The client reads that RFC 9728 document (`resource`, `authorization_servers`). For a resource with a path, it is published at the derived path: `https://example.com/mcp` publishes at `https://example.com/.well-known/oauth-protected-resource/mcp`.
3. It reads the authorization server's metadata (RFC 8414 or OpenID Connect discovery), requires its `issuer` to equal the issuer it started from, and refuses to continue unless `code_challenge_methods_supported` is listed.
4. It obtains a client ID (pre-registration, a Client ID Metadata Document, or dynamic registration), runs the authorization-code flow with PKCE (`S256`) and `resource=<your URI>` (RFC 8707), and sends `Authorization: Bearer <access token>`.

**Amazon Cognito.** Use a user pool with managed login and a public app client (no secret) whose callback URLs list your clients' redirect URIs; Cognito accepts only `S256`. Accept the access token, bound to your server: on the Essentials and Plus feature plans, managed login accepts `resource` (resource binding) and sets the access token's `aud` to that URL, which the configuration above checks. Without resource binding an access token has no `aud`, only `client_id`; the closest check is `client_id` (the app client ID as the audience, which the verifier matches when `aud` is absent) plus a scope of a resource server named after your server, checked in the policy. Never accept the ID token: its `aud` is the app client ID, so it was never issued for your server. A user pool offers no dynamic registration and no Client ID Metadata Documents, and its discovery document has been reported without `code_challenge_methods_supported`: check yours. The usual answer is to publish authorization-server metadata yourself at `https://mcp.example.com/.well-known/oauth-authorization-server`, with your own origin as `issuer` and in `ProtectedResource`'s authorization servers:

```json
{
  "issuer": "https://mcp.example.com",
  "authorization_endpoint": "https://auth.example.com/oauth2/authorize",
  "token_endpoint": "https://auth.example.com/oauth2/token",
  "registration_endpoint": "https://mcp.example.com/register",
  "response_types_supported": ["code"],
  "grant_types_supported": ["authorization_code", "refresh_token"],
  "code_challenge_methods_supported": ["S256"],
  "token_endpoint_auth_methods_supported": ["none"],
  "scopes_supported": ["openid", "orders"]
}
```

The endpoints are your managed-login domain; tokens still come from the pool and carry its issuer, which the verifier checks. A static `/register` answers every registration with the one pre-registered public client: `201` and `{"client_id": "…", "token_endpoint_auth_method": "none", "redirect_uris": […]}`. `Server` serves only the resource metadata: serve these two answers from a CloudFront Function or a small `Api` function on those paths, and give users the client ID for clients that let them enter one.

**Other issuers.** The server side is the same verifier with the provider's issuer, key set and the audience it writes for your server; the verifier accepts RS256 only, so check how the provider signs access tokens. How clients obtain a client ID and a token with your audience differs per provider (Auth0, Okta, Microsoft Entra ID): see the guide's MCP chapter. A pasted access token works with no server change until it expires.

## Tools from an OpenAPI document

With `mcp-openapi`, each selected operation of an OpenAPI 3 JSON document is a tool, and each call is one request to your API:

```rust
use davidrs::client::{self, Limits};
use davidrs::http::{HeaderName, HeaderValue};
use davidrs::mcp::openapi::OpenApi;
use davidrs::RuntimeError;
use serde_json::Value;

/// The API's contract, compiled in so the tools cannot drift from the build.
const DOCUMENT: &str = r#"{
  "openapi": "3.1.0",
  "paths": {
    "/orders/{id}": {
      "get": { "operationId": "getOrder", "summary": "One order", "tags": ["assistant"],
               "parameters": [{ "name": "id", "in": "path", "schema": { "type": "string" } }] },
      "delete": { "operationId": "deleteOrder", "tags": ["admin"] }
    }
  }
}"#;

/// The assistant's read operations, each call sent to `api_url` with this
/// server's own service key.
fn order_tools(api_url: String, service_key: &str) -> Result<OpenApi, RuntimeError> {
    let document: Value = serde_json::from_str(DOCUMENT)
        .map_err(|error| RuntimeError::other("reading the OpenAPI document", error))?;
    let key = HeaderValue::from_str(service_key)
        .map_err(|error| RuntimeError::other("the service key is not a header value", error))?;
    Ok(OpenApi::new(&document, api_url, client::build(Limits::default())?)?
        .select(|operation| operation.has_tag("assistant") && operation.method() == "GET")
        .header(HeaderName::from_static("x-api-key"), key)
        .response_limit(256 * 1024))
}
```

Add them with `Server::new(..).openapi(order_tools(required_env("API_URL")?, &service_key)?)`, the key read with `secrets::string` in `main`. `select` filters on `method()`, `path()`, `has_tag(..)`, `name()` or `definition()` (for an extension such as `x-internal`). The tool's name comes from the `operationId` (an operation without one is not a tool), its description from `description` or `summary`, and its one argument object holds the path, query and header parameters and a JSON body's properties, with local `$ref`s inlined. The document's `servers` are ignored: every call goes to the base URL, and a path that does not start with `/` fails `OpenApi::new`. `GET` tools are marked read-only and `DELETE` destructive. The server checks only that required arguments are present.

| API answer | Tool result |
| --- | --- |
| `2xx` with JSON | the JSON; an object is also `structuredContent` |
| `2xx` with another body, or none | the body as text, or `HTTP 204` |
| `4xx` | `isError`, `UPSTREAM_REFUSED` with the API's status and body |
| `5xx`, a redirect, no answer | `isError`, `UPSTREAM_UNAVAILABLE`, nothing about the failure |
| larger than `response_limit` (1 MiB by default) | `isError`, `ERROR_LIMIT_EXCEEDED` |
| slower than the client's `Limits` | `isError`, `ERROR_TIMEOUT` |

**Tokens that cross services.** `forward_caller_token()` instead of `header(..)` sends the caller's own `Authorization` header, so the API authorizes each call as it would the application's. Use it only when the API is your own (same owner, same authorization server) and accepts tokens issued for this server's resource URI, for example by listing it among its audiences. Anywhere else a forwarded token makes the server a confused deputy, which the MCP specification forbids. Otherwise send the server's own credential with `header`, write the tool by hand with a client-credentials token plus the user's identity as data, or exchange the caller's token (RFC 8693) where your authorization server supports it.

## Visibility, rate limits and origins

Here `policy` is an `Access<User, User>` whose `User` carries an `admin` flag read with `claims.contains("cognito:groups", "admin")`, `ddb` a DynamoDB client, and `close_month` a handler that checks `context.scope().caller().admin` again itself: the list is a courtesy to the model, the check is what protects the data.

```rust
use std::time::Duration;

use davidrs::digest::sha256_hex;
use davidrs::http::access::Grant;
use davidrs::http::rate_limit::DynamoWindow;
use davidrs::http::{RateLimitConfig, RateLimited, Request};
use davidrs::mcp::{Server, Tool};
use serde_json::json;

let limit = RateLimitConfig::new("mcp", 120, Duration::from_secs(60));
let credential = |request: &Request<'_>| {
    sha256_hex(request.header("authorization").unwrap_or("anonymous").as_bytes())
};
let server = Server::new("books", "1.0.0", policy)
    .admission(RateLimited::new(DynamoWindow::new(ddb, "rate-limits"), limit).key(credential))
    .allow_origins(["https://app.example.com"])
    .tool(Tool::new("close_month", "Closes the current month", json!({ "type": "object" }), close_month))
    .visible(|tool, grant: &Grant<User, ()>| tool != "close_month" || grant.caller().admin);
```

- A hidden tool is absent from `tools/list` and unknown to `tools/call`; with a rule the list is marked `cacheScope: "private"`.
- `admission` counts every request before it is parsed (the specification asks servers to rate limit tool calls); a refusal is `429` with its budget headers. There is no verified caller yet, so count under a hash of the credential, never the credential itself; behind CloudFront the source address is CloudFront's, and behind an HTTP API `Claims::from_gateway` gives the verified subject. `DynamoWindow` needs `dynamodb:UpdateItem` on its table, with TTL enabled on `ttl`.
- `allow_origins` lists the browser origins allowed to call; a request with any other `Origin` is `403` before anything is parsed. None are allowed by default, and clients that send no `Origin` are unaffected.
- `body_limit(bytes)` caps the request body, and so a call's arguments (1 MiB by default).

## Testing with `Server::handle`

`Server::handle` serves one hand-built request through the real policy. `test_support` (dev-dependency feature `test-support`) builds it; a request must repeat its revision, method and tool name in headers. Appended to the API-key server's `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use davidrs::digest::sha256_hex;
    use davidrs::http::{Body, HeaderValue, StatusCode};
    use davidrs::mcp::{Server, Tool, PROTOCOL_VERSION};
    use davidrs::test_support;
    use serde_json::{json, Value};

    use super::{get_price, price_schema, ApiKeys, App};

    #[tokio::test]
    async fn an_unknown_item_is_a_result_the_model_can_read() {
        let keys = ApiKeys { hashes: HashSet::from([sha256_hex(b"test-key")]) };
        let server = Server::new("catalogue", "1.0.0", keys)
            .tool(Tool::new("get_price", "The price of one item", price_schema(), get_price));
        let meta = json!({
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        let params = json!({ "name": "get_price", "arguments": { "item": "lamp" }, "_meta": meta });
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params });
        let mut request = test_support::post_json("https://mcp.example.com/", &body.to_string());
        let headers = request.headers_mut();
        headers.insert("authorization", HeaderValue::from_static("Bearer test-key"));
        headers.insert("mcp-protocol-version", HeaderValue::from_static(PROTOCOL_VERSION));
        headers.insert("mcp-method", HeaderValue::from_static("tools/call"));
        headers.insert("mcp-name", HeaderValue::from_static("get_price"));

        let app = Arc::new(App { prices: vec![("pen".to_owned(), 150)] });
        let response = server.handle(app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let Body::Text(text) = response.body() else { panic!("a JSON body") };
        let answer: Value = serde_json::from_str(text).expect("a JSON-RPC answer");
        assert_eq!(answer["result"]["isError"], true);
        assert_eq!(answer["result"]["content"][0]["text"], "ERROR_UNKNOWN_ITEM: No item with this name");
    }
}
```

## Deploying

MCP clients send plain HTTPS requests with a bearer token and cannot sign with SigV4, which leaves two shapes. Build either with `cargo lambda build --release --arm64 --output-format zip`.

- **CloudFront in front of a Function URL with auth type `NONE`.** CloudFront gives the custom domain and AWS WAF, the only firewall a Function URL can have; reserved concurrency is its only brake. Origin access control does not fit: a `POST` through it needs the body's SHA-256 in `x-amz-content-sha256`, which MCP clients do not send. So the function verifies the bearer itself on every request (`gateway_claims(false)`), which is also what lets its `401` carry the metadata pointer. Allow every method, disable caching, and forward all viewer headers except `Host` (the managed `AllViewerExceptHostHeader` origin request policy) so `Authorization`, `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` arrive. A call may run as long as CloudFront's origin response timeout (30 s by default, adjustable per origin). Auth type `NONE` still needs a resource-based policy for `*`: `lambda:InvokeFunctionUrl` under `lambda:FunctionUrlAuthType` = `NONE`, and `lambda:InvokeFunction` under `lambda:InvokedViaFunctionUrl` = `true`, so the statement opens the URL and not the `Invoke` API. The URL stays reachable directly: a caller can skip WAF, never the token check.
- **API Gateway HTTP API with a JWT authorizer.** The gateway verifies signature, issuer, audience, expiry and scopes before the function runs, and `Access` reads the claims by default. Each call must finish within the 30 s integration timeout. The gateway's own `401` does not name the metadata, so route `GET /.well-known/oauth-protected-resource` and the same path followed by the endpoint's path to the function without the authorizer. The authorizer's audience must match the token's `aud` (or `client_id` when a token has none). For WAF, put CloudFront in front.
  Behind it the server verifies nothing again: build `Access` without `verify_bearer`, map the claims the authorizer passes (for Cognito `sub`, `cognito:groups`, `scope`), and add `.gateway_token_claims(true)` when a claim is a nested object.

In both, the answer is buffered: any `POST` path is the MCP endpoint, a `GET` of the metadata path is the metadata, and everything else is `405`.

## Connecting a client

Try the server with `curl`, repeating the revision and method in headers:

```bash
curl -s https://mcp.example.com/ \
  -H "Authorization: Bearer $ORDERS_MCP_KEY" -H 'Content-Type: application/json' \
  -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/list' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
claude mcp add --transport http orders https://mcp.example.com/ --header "Authorization: Bearer $ORDERS_MCP_KEY"
```

A client with OAuth support needs no header: it follows the `401` to your issuer and signs the user in.

## What it answers, and what it does not do

- Methods `server/discover`, `ping`, `tools/list` and `tools/call`; a notification gets `202`. JSON-RPC errors: `-32700` (not JSON), `-32600` (not one request), `-32601` (unknown method, `404`), `-32602` (unknown or hidden tool with `200`; arguments not an object, or `_meta` without the revision and client capabilities, with `400`), `-32020` (a mirrored header is missing or disagrees with the body), `-32022` (another revision or none, listing the one supported).
- No streaming (no progress, subscriptions or list-change notifications; the tool list is fixed for the life of an instance), and tools only: no resources, prompts, completions or input-required results.
- No sessions, no CORS headers (a browser client also needs CORS and preflights, which CloudFront can answer), no schema validation, no `outputSchema` and no `tools/list` pagination.
- OpenAPI narrowly: version 3 JSON documents, JSON request bodies, local `$ref`s; no cookie parameters and no argument for a header the transport sets (`Accept`, `Content-Type`, `Authorization`, `Host`, `Content-Length`, `Transfer-Encoding`).
- No authorization server (no tokens issued, no clients registered, no authorization-server metadata) and no scope step-up: a `403` carries no `insufficient_scope` challenge.

Guide: [MCP servers](https://docs.rs/davidrs/latest/davidrs/guide/mcp/index.html), [Bearer tokens](https://docs.rs/davidrs/latest/davidrs/guide/tokens/index.html), [Access control](https://docs.rs/davidrs/latest/davidrs/guide/access/index.html), [Security on AWS](https://docs.rs/davidrs/latest/davidrs/guide/aws_security/index.html). Example: [`examples/mcp.rs`](https://github.com/eusoumaxi/davidrs/blob/main/examples/mcp.rs). The SDK clients and secrets a tool uses: [aws-services.md](aws-services.md); `Access` policies and `RateLimited`: [http.md](http.md).
