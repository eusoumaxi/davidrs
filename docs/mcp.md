# MCP servers

Enable `mcp` for tools you write as async functions, and `mcp-openapi` when an OpenAPI document should become those tools. Add `auth` when the function verifies bearer tokens, `digest` when callers present API keys stored as SHA-256 hashes, and `logs` so a failed tool's code is logged. An AI client then calls your system with the signed-in user's own permissions.

This chapter is long. The path through it is short:

1. [One Lambda, running in minutes](#one-lambda-running-in-minutes) creates the function and connects a client.
2. [With an API key](#with-an-api-key) is the credential for automation. [With Amazon Cognito](#with-amazon-cognito-or-any-openid-connect-issuer) (or any OpenID Connect issuer) is the credential when a person is signed in and the client can follow a `401`.
3. [Deploying it](#deploying-it) is a Function URL with auth type `NONE` behind CloudFront, or an HTTP API with a JWT authorizer. Origin access control does not fit, because MCP clients do not send `x-amz-content-sha256`. [Security on AWS](crate::guide::aws_security) explains that constraint.
4. The protocol sections after the quick start are for a client that fails in a way those three do not explain: a header that disagrees with the body, an error in the wrong channel, or a tool result that leaked.

The Model Context Protocol (MCP) is how those clients find and call tools that live outside them. An MCP server publishes a list of tools, each with a name, a description a model reads and a JSON Schema for its arguments, and runs a tool when a client calls it. [`mcp::Server`](crate::mcp::Server) is such a server in one Lambda function. It speaks MCP revision 2026-07-28 ([`PROTOCOL_VERSION`](crate::mcp::PROTOCOL_VERSION)) over Streamable HTTP, answers each request with one JSON object and keeps no session, so it scales like any other function.

If your API has an OpenAPI document, its operations become tools in a few lines. A tool that needs several requests, or data your API does not serve, is an async function.

## Why it exists

The protocol is small, but a server that gets it slightly wrong fails in ways a model cannot recover from:

- **Headers that disagree with the body.** A request repeats its protocol revision, its method and a tool's name in headers, so a gateway can route without parsing. A server that runs the body without comparing lets a proxy route one call and execute another. Here any mismatch is a `400` with `-32020`.
- **Errors in the wrong channel.** A model reads a tool result marked `isError` and corrects itself; a JSON-RPC error usually ends its turn. Here bad arguments, refusals and API failures are results, and only malformed requests are JSON-RPC errors.
- **Leaks through results.** A result reaches the model and often the user. A tool's `5xx` shows a fixed message, exactly as an HTTP handler's [`Failure`](crate::http::Failure) does.
- **An OAuth client with nowhere to go.** A `401` without `WWW-Authenticate: Bearer resource_metadata="…"` leaves a client unable to find where to sign in. Here every `401` points at the RFC 9728 metadata the server publishes.
- **Unbounded work.** The request body, the number of tools, the API answers an OpenAPI tool reads and each call's time are bounded, and a tool that runs out of time still returns a result under its JSON-RPC id instead of a bare `504`.

## One Lambda, running in minutes

An MCP server does not need a platform of its own. One Rust function behind a URL is a complete server: it answers `tools/list` and `tools/call` in microseconds of its own time, starts cold in about a tenth of a second, costs nothing while idle, and deploys with the rest of your API.

### Why a function rather than a managed gateway

A managed MCP gateway, such as Amazon Bedrock AgentCore Gateway, sits between the client and your tools and calls them in turn. That buys features — many heterogeneous targets behind one endpoint, managed outbound credentials for third-party APIs, semantic search over hundreds of tools — at the price of another service in every call: its hop, its configuration kept in step with your API, and its limits. For tools over your own API, the function is simpler and faster:

- **One hop.** Client → (CloudFront) → your function → your API. Nothing else to configure, pay for or wait on.
- **Tools live with the code.** Names, descriptions and who sees what are Rust, reviewed and versioned with the API they call; there is no second copy to keep in sync.
- **The authentication you already have.** The same Cognito pool, OAuth issuer or API keys your API trusts.
- **Rust start-up.** No runtime to boot and a small binary: the cold start is the platform's, and a warm call's time is your API's.

Reach for a gateway when you need what it adds, not by default.

### From zero to a connected client

1. Create the function and depend on the crate:

   ```bash
   cargo lambda new orders-mcp
   ```

   ```toml
   [dependencies]
   davidrs = { version = "0.1", default-features = false, features = ["mcp-openapi", "auth", "digest", "logs"] }
   serde = { version = "1", features = ["derive"] }
   serde_json = "1"
   tokio = { version = "1", features = ["macros"] }
   ```

2. Choose who may call — an API key or your identity provider — and write `main.rs` (below).
3. Build and deploy: `cargo lambda build --release --arm64 --output-format zip`, then a Function URL with auth type `NONE` behind CloudFront, or an API Gateway HTTP API (see [Deploying it](#deploying-it)).
4. Connect a client:

   ```bash
   claude mcp add --transport http orders https://mcp.example.com/ --header "Authorization: Bearer $ORDERS_MCP_KEY"
   ```

   A client with OAuth support needs no header: it follows the `401` to your issuer and signs the user in.

### With an API key

The quickest start, and the right credential for automation: a long-lived key per caller. Store only the SHA-256 of each key — in Secrets Manager or the function's environment — and compare hashes, so a leaked configuration leaks no key:

```rust,no_run
use std::collections::HashSet;
use std::sync::Arc;

use davidrs::digest::sha256_hex;
use davidrs::http::{codes, Failure, Policy, Request, StatusCode};
use davidrs::mcp::{Server, Tool};
use davidrs::{Context, Invocation, RuntimeError};
use serde::Deserialize;

/// The hashes of the keys allowed to call, read once at cold start.
struct ApiKeys {
    hashes: HashSet<String>,
}

impl Policy for ApiKeys {
    type Scope = ();

    async fn authorize(&self, request: &Request<'_>, _invocation: &Invocation) -> Result<(), Failure> {
        let key = request.header("authorization").map(davidrs::auth::bearer);
        match key {
            Some(key) if self.hashes.contains(&sha256_hex(key.as_bytes())) => Ok(()),
            _ => Err(Failure::new(StatusCode::UNAUTHORIZED, codes::UNAUTHENTICATED, "A valid API key is required")),
        }
    }
}

/// `{"status": "open"}`: which orders to count.
#[derive(Deserialize)]
struct Count {
    status: String,
}

async fn count(_app: Arc<()>, input: Count, _context: Context<()>) -> Result<serde_json::Value, Failure> {
    Ok(serde_json::json!({ "status": input.status, "count": 3 }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders-mcp")?;
    let keys = ApiKeys {
        hashes: davidrs::list_env("API_KEY_HASHES")?.into_iter().collect(),
    };
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "status": { "type": "string", "enum": ["open", "shipped"] } },
        "required": ["status"]
    });
    Server::new("orders", "1.0.0", keys)
        .tool(Tool::new("count_orders", "Counts the caller's orders in one status", schema, count))
        .run(Arc::new(()))
        .await
}
```

### With Amazon Cognito (or any OpenID Connect issuer)

Replace the policy with [`Access`](crate::http::access::Access) verifying the bearer against your user pool, and publish the resource metadata, so OAuth clients find where to sign in; the tools do not change:

```rust,no_run
use std::sync::Arc;

use davidrs::auth::{Verifier, VerifierConfig};
use davidrs::client::{self, Limits};
use davidrs::http::access::{Access, Claims};
use davidrs::mcp::{ProtectedResource, Server};
use davidrs::RuntimeError;

/// The caller: the subject Cognito verified.
fn user(claims: &Claims) -> Option<String> {
    claims.subject().map(str::to_owned)
}

# async fn run() -> Result<(), RuntimeError> {
let issuer = davidrs::required_env("COGNITO_ISSUER")?;
let resource = davidrs::required_env("MCP_URL")?;
let tokens = VerifierConfig::new(issuer.clone(), format!("{issuer}/.well-known/jwks.json"))
    .with_audiences(vec![resource.clone()])
    .with_required_claim("token_use", "access");
let verifier = Arc::new(Verifier::load(client::build(Limits::default())?, tokens).await?);

Server::new("orders", "1.0.0", Access::new(user).require_caller().verify_bearer(verifier))
    .protected_resource(ProtectedResource::new(resource, [issuer]))
    .run(Arc::new(()))
    .await
# }
```

The [Amazon Cognito](#amazon-cognito) section below covers the user pool side: the app client, resource binding, and what to publish because Cognito has no dynamic client registration.

### Trying it without a client

A request names the protocol revision in its body and repeats it, with the method, in headers:

```bash
curl -s https://mcp.example.com/ \
  -H "Authorization: Bearer $ORDERS_MCP_KEY" -H 'Content-Type: application/json' \
  -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/list' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

## Hand-written tools

A [`Tool`](crate::mcp::Tool) is a name, a description, the JSON Schema of its arguments, and a handler shaped like an HTTP handler: the application state, the arguments deserialized into your type, and a [`Context`](crate::Context) whose scope is what the server's policy established. [`Server::handle`](crate::mcp::Server::handle) serves one hand-built request, which is how you test a server:

```rust
use std::sync::Arc;

use davidrs::http::{Failure, Public, StatusCode};
use davidrs::mcp::{Server, Tool, PROTOCOL_VERSION};
use davidrs::Context;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// What every tool can reach: clients, tables, configuration.
struct App {
    prices: Vec<(String, u32)>,
}

#[derive(Deserialize)]
struct Query {
    item: String,
}

#[derive(Serialize)]
struct Price {
    item: String,
    cents: u32,
}

/// Looks one item up; an unknown item is a refusal the model can read.
async fn price(app: Arc<App>, query: Query, _context: Context<()>) -> Result<Price, Failure> {
    app.prices
        .iter()
        .find(|(item, _)| *item == query.item)
        .map(|(item, cents)| Price { item: item.clone(), cents: *cents })
        .ok_or_else(|| Failure::new(StatusCode::NOT_FOUND, "ERROR_UNKNOWN_ITEM", "No such item"))
}

let schema = json!({
    "type": "object",
    "properties": { "item": { "type": "string", "description": "The item's name" } },
    "required": ["item"],
    "additionalProperties": false
});
let server = Server::new("prices", "1.0.0", Public)
    .instructions("Prices are in cents.")
    .tool(
        Tool::new("price", "The price of one item, in cents", schema, price)
            .annotations(json!({ "readOnlyHint": true })),
    );

let call = json!({
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
        "name": "price",
        "arguments": { "item": "pen" },
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}
        }
    }
});
let request = lambda_http::http::Request::builder()
    .method("POST")
    .uri("https://mcp.example.com/")
    .header("mcp-protocol-version", PROTOCOL_VERSION)
    .header("mcp-method", "tools/call")
    .header("mcp-name", "price")
    .body(lambda_http::Body::Text(call.to_string()))
    .unwrap();
let app = Arc::new(App { prices: vec![("pen".to_owned(), 150)] });

# tokio::runtime::Runtime::new().unwrap().block_on(async {
let response = server.handle(app, request).await;
let lambda_http::Body::Text(body) = response.body() else { panic!("a JSON body") };
let answer: Value = serde_json::from_str(body).unwrap();
assert_eq!(answer["result"]["structuredContent"], json!({ "item": "pen", "cents": 150 }));
assert_eq!(answer["result"]["isError"], false);
# });
```

What the server does with a handler:

- **Arguments** are deserialized into the handler's type. Arguments that do not fit are refused with [`INVALID_ARGUMENTS`](crate::mcp::INVALID_ARGUMENTS) before the handler runs, as a result the model can read and correct. The schema is what the model writes arguments from, so describe each property, and keep it in step with the type: the server neither generates it nor validates against it.
- **A success** that serializes to a JSON object is returned as `structuredContent` and, serialized, as text; a string is returned as text.
- **A [`Failure`](crate::http::Failure)** becomes a result with `isError: true` whose text is its code and public message; a `5xx` shows the fixed `InternalServerError`, and its detail is never sent.
- **Time.** The handler runs under the invocation deadline less [`CALL_MARGIN`](crate::mcp::CALL_MARGIN) (one second), and [`Context::deadline`](crate::Context::deadline) already reflects it, so a child budget derived from it leaves time for the answer. A handler still running at its deadline is dropped and answered with `ERROR_TIMEOUT`.

Annotations such as `readOnlyHint` are hints: a client may skip a confirmation for a read-only tool, but must treat them as untrusted, and they never replace a check in the handler.

## Tools from an OpenAPI document

With the `mcp-openapi` feature, [`OpenApi`](crate::mcp::openapi::OpenApi) reads an OpenAPI 3 document and turns each operation you select into a tool. A call is one request to your API. With [`forward_caller_token`](crate::mcp::openapi::OpenApi::forward_caller_token) it carries the caller's own `Authorization` header, so your API authorizes it exactly as it would a request from your application and the server adds no permission model of its own; without it, a call carries only the headers you configure with [`header`](crate::mcp::openapi::OpenApi::header), such as a service key. This is a complete function, with the caller's token verified here and forwarded to your own API:

```rust,no_run
use std::sync::Arc;

use davidrs::auth::{Verifier, VerifierConfig};
use davidrs::client::{self, Limits};
use davidrs::http::access::{Access, Claims};
use davidrs::mcp::openapi::OpenApi;
use davidrs::mcp::{ProtectedResource, Server};
use davidrs::{required_env, RuntimeError};
use serde_json::Value;

/// This server's canonical URI: what clients are configured with, and the
/// audience every token it accepts must carry.
const RESOURCE: &str = "https://mcp.example.com";

/// The authorization server whose tokens it accepts.
const ISSUER: &str = "https://id.example.com";

/// The API's contract, compiled in so the tools cannot drift from the build.
const DOCUMENT: &str = r#"{
  "openapi": "3.1.0",
  "paths": {
    "/orders": {
      "get": { "operationId": "listOrders", "summary": "The caller's orders", "tags": ["assistant"] }
    },
    "/orders/{id}": {
      "get": {
        "operationId": "getOrder",
        "summary": "One order",
        "tags": ["assistant"],
        "parameters": [{ "name": "id", "in": "path", "schema": { "type": "string" } }]
      },
      "delete": { "operationId": "deleteOrder", "tags": ["admin"] }
    }
  }
}"#;

/// Whoever the token is about.
fn subject(claims: &Claims) -> Option<String> {
    claims.subject().map(str::to_owned)
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let http = client::build(Limits::default())?;
    let tokens = VerifierConfig::new(ISSUER, format!("{ISSUER}/.well-known/jwks.json"))
        .with_audiences(vec![RESOURCE.to_owned()]);
    let verifier = Arc::new(Verifier::load(http.clone(), tokens).await?);

    let document: Value = serde_json::from_str(DOCUMENT)
        .map_err(|error| RuntimeError::other("reading the OpenAPI document", error))?;
    let api = OpenApi::new(&document, required_env("API_URL")?, http)?
        .select(|operation| operation.has_tag("assistant"))
        .forward_caller_token();

    let policy = Access::new(subject)
        .gateway_claims(false)
        .verify_bearer(verifier)
        .require_caller();
    Server::new("orders", "1.0.0", policy)
        .instructions("The signed-in user's orders. Amounts are in cents.")
        .protected_resource(ProtectedResource::new(RESOURCE, [ISSUER]).scopes(["orders"]))
        .openapi(api)
        .run(Arc::new(()))
        .await
}
```

The [module documentation](crate::mcp::openapi) lists how an operation becomes a tool: its name comes from the `operationId`, its description from `description` or `summary`, and its one argument object holds the path, query and header parameters and the properties of a JSON body, with local `$ref`s inlined. Select deliberately, with [`select`](crate::mcp::openapi::OpenApi::select) on a method, a tag or an extension: every tool costs the model context on every turn, and a server holds at most [`MAX_TOOLS`](crate::mcp::MAX_TOOLS) (128).

What the API answers becomes the result:

| API answer | Tool result |
| --- | --- |
| `2xx` with JSON | the JSON; an object is also `structuredContent` |
| `2xx` with another body, or none | the body as text, or `HTTP 204` |
| `4xx` | `isError`, [`UPSTREAM_REFUSED`](crate::mcp::openapi::UPSTREAM_REFUSED) with the API's status and body, which the caller could have read directly |
| `5xx`, a redirect, no answer | `isError`, [`UPSTREAM_UNAVAILABLE`](crate::mcp::openapi::UPSTREAM_UNAVAILABLE), with nothing about the failure |
| larger than the limit (1 MiB) | `isError`, `ERROR_LIMIT_EXCEEDED` |
| slower than the client's limit | `isError`, `ERROR_TIMEOUT` |

Forwarding a token has consequences: read [tokens that cross services](#tokens-that-cross-services) before pointing a server at an API.

## Who may call, and what they see

The server's policy is any HTTP [`Policy`](crate::http::Policy), usually an [`Access`](crate::http::access::Access) policy; the [access chapter](crate::guide::access) explains it. It authorizes every `POST`, and nothing else: the metadata a client needs before it has a token is public. Its scope reaches every tool handler, and [`Server::visible`](crate::mcp::Server::visible) decides per caller which tools exist at all:

```rust
use std::sync::Arc;

use davidrs::http::access::{Access, Claims, Grant};
use davidrs::http::{Failure, StatusCode};
use davidrs::mcp::{Server, Tool};
use davidrs::Context;
use serde_json::{json, Value};

struct User {
    id: String,
    admin: bool,
}

fn user(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.subject()?.to_owned(),
        admin: claims.contains("cognito:groups", "admin"),
    })
}

/// Hidden from other callers, and checked again here: the list is a
/// courtesy to the model, the check is what protects the books.
async fn close_month(_app: Arc<()>, _arguments: Value, context: Context<Grant<User, ()>>) -> Result<String, Failure> {
    let user = context.scope().caller();
    if !user.admin {
        return Err(Failure::new(StatusCode::FORBIDDEN, "ERROR_FORBIDDEN", "Administrators only"));
    }
    Ok(format!("Closed by {}", user.id))
}

let server = Server::new("books", "1.0.0", Access::new(user).require_caller())
    .tool(Tool::new("close_month", "Closes the current month", json!({ "type": "object" }), close_month))
    .visible(|tool, grant: &Grant<User, ()>| tool != "close_month" || grant.caller().admin);
# let _ = server;
```

A hidden tool is absent from `tools/list` and unknown to `tools/call`. The list then differs between callers, so it is marked `cacheScope: "private"` and clients do not share it. [`Server::admission`](crate::mcp::Server::admission) counts requests before anything is parsed: the specification asks servers to rate limit tool calls, and a [`RateLimited`](crate::http::RateLimited) admission does.

## Deploying it

MCP clients send plain HTTPS requests with a bearer token; they cannot sign requests with AWS SigV4. Two shapes follow from that.

**CloudFront in front of a Function URL whose auth type is `NONE`.** CloudFront gives the server a custom domain and AWS WAF — the web ACL on the distribution is the only firewall an MCP Function URL can have, and reserved concurrency on the function is its only brake (see the [security chapter](crate::guide::aws_security)); the Function URL invokes the function directly, and a call may run as long as CloudFront's origin response timeout allows (30 seconds by default, adjustable per origin). Origin access control does not fit: for a `POST` it needs the client to send the SHA-256 of the body in `x-amz-content-sha256`, which MCP clients do not do. The function therefore verifies the bearer itself on every request, as above, which is also what lets its `401` carry the metadata pointer. Configure the behaviour with every method allowed, caching disabled, and an origin request policy that forwards all viewer headers except `Host` (the managed `AllViewerExceptHostHeader`), so `Authorization`, `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` reach the function. The Function URL stays reachable without CloudFront, so a caller can skip what CloudFront adds, such as WAF rules; it cannot skip the token check.

**API Gateway HTTP API with a JWT authorizer.** The gateway verifies the token — signature, issuer, audience, expiry and the route's scopes — before the function runs, so unauthenticated traffic costs no invocation, and `Access` reads the verified claims by default: the function verifies nothing again and only maps the token's claims, as [tokens the gateway already verified](crate::guide::access#tokens-the-gateway-already-verified) shows for Cognito, Auth0, Okta, Entra ID, Keycloak and Clerk. For a web ACL, put CloudFront in front: a web ACL cannot attach to an HTTP API. Three things to know:

- Each call must finish within the 30 seconds an HTTP API waits for its integration.
- The gateway's own `401` does not name the resource metadata. A client that receives it falls back to probing `/.well-known/oauth-protected-resource` followed by the endpoint's path, then at the root: route both `GET` paths to the same function without the authorizer.
- The authorizer's audiences must match the token's `aud`; for a token without `aud`, API Gateway compares its `client_id` instead.

In both shapes the answer is buffered; the server never streams. Any `POST` path is the MCP endpoint, a `GET` of the metadata path is the metadata, and every other request is `405`, so the gateway or the distribution decides which paths reach the function.

## Authentication

### How a client signs in

MCP authorization is OAuth 2.1, discovered from the server itself:

1. The client calls without a token and receives `401` with `WWW-Authenticate: Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource"`, plus `scope="…"` when you configured scopes.
2. It reads that document (RFC 9728). `resource` is your server's canonical URI, `authorization_servers` the issuers you trust. [`ProtectedResource`](crate::mcp::ProtectedResource) publishes it at the path RFC 9728 derives from the URI: `https://example.com/mcp` publishes at `https://example.com/.well-known/oauth-protected-resource/mcp`.
3. It fetches the authorization server's metadata from the issuer — RFC 8414 (`/.well-known/oauth-authorization-server`) or OpenID Connect discovery (`/.well-known/openid-configuration`) — requires its `issuer` to equal the issuer it started from, and refuses to continue unless `code_challenge_methods_supported` is listed.
4. It obtains a client ID: one it was configured with (pre-registration), an HTTPS URL describing itself when the metadata says `client_id_metadata_document_supported` (a Client ID Metadata Document), or dynamic registration (RFC 7591) at `registration_endpoint`.
5. It runs the authorization-code flow with PKCE (`S256`) in the user's browser, and sends `resource=<your resource URI>` (RFC 8707) in both the authorization and the token request.
6. It sends `Authorization: Bearer <access token>` on every request.

Your server's part: verify every token, accept only tokens issued for this server — their `aud` names your resource URI, as the verifier above requires — and answer anything else with `401`. The server adds the challenge to every `401` the policy returns.

### Amazon Cognito

A user pool with managed login is an OAuth 2.1 authorization server: authorization-code grants with PKCE (Cognito accepts only `S256`), and a public app client, without a secret, for MCP clients, whose callback URLs list the redirect URIs your clients use.

**Which token to accept: the access token, bound to your server.** On the Essentials and Plus feature plans, managed login accepts the `resource` parameter MCP clients send (resource binding, RFC 8707) and sets the access token's `aud` to that URL; refreshed tokens keep it. That is exactly the audience check MCP asks for:

```rust
use davidrs::auth::VerifierConfig;

let issuer = "https://cognito-idp.us-east-1.amazonaws.com/us-east-1_EXAMPLE";
let config = VerifierConfig::new(issuer, format!("{issuer}/.well-known/jwks.json"))
    .with_audiences(vec!["https://mcp.example.com".to_owned()])
    .with_required_claim("token_use", "access");
# let _ = config;
```

Check that your pool's plan includes resource binding: without it, an access token carries no `aud`, only `client_id`, and the closest check is `client_id` plus a scope of a resource server named after your server. Do not accept the ID token instead: it is issued _to the client_, to say who signed in, and its `aud` is the app client ID, so accepting it means accepting a token that was never issued for your server. The access token is the one OAuth issues for calling a resource; it carries the scopes and `cognito:groups`, and a pre token generation trigger (Essentials or Plus) can add claims your policy needs.

**What Cognito lacks, and the usual answer.** A user pool offers no dynamic client registration (RFC 7591) and does not advertise Client ID Metadata Documents. Its discovery document, under the issuer at `https://cognito-idp.<region>.amazonaws.com/<pool-id>/.well-known/openid-configuration`, has been reported without `code_challenge_methods_supported` even though Cognito enforces `S256`; check yours, because MCP clients refuse to continue without it. The usual answer is to publish the authorization-server metadata yourself, next to the MCP server, and name your own origin in `authorization_servers`:

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

served at `https://mcp.example.com/.well-known/oauth-authorization-server`, with a static `/register` that answers every registration with the one pre-registered public client: `201` and `{"client_id": "…", "token_endpoint_auth_method": "none", "redirect_uris": […]}`. The endpoints are your managed-login domain; tokens still come from the pool and carry the pool's issuer, which is what your verifier checks. The document's `issuer` is your origin because a client requires it to equal the URL it found the document from; a client never compares it with a token's `iss`. Every client receives the same client ID, so its callback URLs must list every redirect URI your supported clients use; a client whose redirect URI is missing fails at the authorization endpoint. The 2026-07-28 revision prefers pre-registration and Client ID Metadata Documents, and keeps dynamic registration optional, so also give users the client ID for clients that let them enter one. [`Server`](crate::mcp::Server) serves only the resource metadata: publish these two answers from a CloudFront Function, or from a small [`Api`](crate::http::Api) function on those paths of the same distribution.

### Other OpenID Connect providers

The server side does not change: a [`Verifier`](crate::auth::Verifier) for the provider's issuer and key set, the audience the provider writes for your server, and RS256 signatures — the only algorithm the verifier accepts, so check that the provider signs access tokens with it. What changes is how a client obtains a client ID and a token with your audience:

- **Auth0.** Access tokens for an API carry the API's identifier in `aud`, requested with Auth0's own `audience` parameter. Check whether your tenant honours the standard `resource` parameter MCP clients send, or set a default audience for the tenant. Dynamic registration is a tenant setting and creates third-party applications; enable it deliberately.
- **Okta.** Use a custom authorization server: its access tokens carry the audience you configure, while the org authorization server's tokens are meant for Okta's own APIs. Okta's registration endpoint is an authenticated management API, not open registration: check what your plan offers, or pre-register a client.
- **Microsoft Entra ID.** There is no dynamic registration: register an application for MCP clients and hand out its ID, or answer registrations statically as above. Tokens for your API carry its application ID URI or client ID in `aud`, requested with scopes such as `api://<app-id>/<scope>` on the v2.0 endpoint, which identifies the API by its scopes; check how your tenant treats the `resource` parameter clients send. Set the API's access-token version to 2 in its manifest (`api.requestedAccessTokenVersion`) so its tokens name the issuer `https://login.microsoftonline.com/<tenant>/v2.0`.

### A pasted token or an API key

Not every client runs OAuth, but most accept a fixed header in their configuration, such as `Authorization: Bearer <token>`:

- **A pasted access token** works with the same verifier and no server change. It expires on the provider's schedule — an hour by default on Cognito — which suits trying a server and wears thin for daily use.
- **An API key** suits automation: a long-lived secret per caller. Write it as your own [`Policy`](crate::http::Policy): look the key up by its SHA-256 (store only hashes), and return the caller it belongs to or a `401`. The server adds the OAuth challenge to that `401` too, so OAuth-capable clients still find the metadata.

### Tokens that cross services

With [`forward_caller_token`](crate::mcp::openapi::OpenApi::forward_caller_token), OpenAPI tools send the caller's token to your API unchanged; without it they send none. The MCP specification forbids a server to pass a token it received through to an upstream API: a token names its audience, and an API that accepts tokens issued for someone else can be driven by anyone holding one (the confused deputy). Forwarding is meant for one arrangement only: the MCP server is a thin front door to your own API — same owner, same authorization server — and the API accepts tokens issued for the MCP server's resource URI, for example by listing it among its audiences. The token then never leaves the system it was issued for. Never forward to someone else's API, or to an API that should not trust tokens issued for the MCP server.

When the API is a separate resource, leave forwarding off and either give the tools the server's own credential with [`header`](crate::mcp::openapi::OpenApi::header), or write the tools by hand and call it with the tool's own credential (a client-credentials token or a service key) plus the user's identity as data the API trusts from that client alone, or exchange the caller's token for one issued for the API (OAuth token exchange, RFC 8693) where your authorization server supports it.

## Limits

| Bound | Default | Set with |
| --- | --- | --- |
| request body, and so a call's arguments | 1 MiB | [`Server::body_limit`](crate::mcp::Server::body_limit) |
| tools per server | 128 | [`MAX_TOOLS`](crate::mcp::MAX_TOOLS) |
| one tool call | the invocation deadline less one second | [`CALL_MARGIN`](crate::mcp::CALL_MARGIN) |
| an API answer an OpenAPI tool reads | 1 MiB | [`OpenApi::response_limit`](crate::mcp::openapi::OpenApi::response_limit) |
| an API call | the client's [`Limits`](crate::client::Limits), inside the call's deadline | [`client::build`](crate::client::build) |

A tool name that is not 1 to 128 ASCII letters, digits, `_`, `-` or `.`, two tools with one name, or more than 128 tools panic when the server is built, so the mistake shows on the first cold start rather than in a client.

## Use cases

- **An assistant for your product's users** that reads and changes their data through your existing API with their own permissions: selected OpenAPI operations, tokens from your user pool.
- **An internal agent's tools** that join several tables into one answer: hand-written tools over the SDK clients in `App`.
- **Role-shaped tool lists**: read tools for everyone, write tools for administrators, with [`Server::visible`](crate::mcp::Server::visible) and a check in each handler.
- **A tool surface for an agent platform** that calls MCP servers with tokens from your authorization server.

## What it does not do

- **One revision.** It speaks 2026-07-28; a request that names another revision, or names none, is refused with `-32022` and the revision it speaks.
- **No streaming.** Every answer is one JSON object: no progress notifications, no `subscriptions/listen`, no list-change notifications. The tool list is fixed for the life of an instance.
- **Tools only.** No resources, prompts or completions, and no input-required results that ask the client for more.
- **No sessions and no CORS.** Browser origins are checked against [`Server::allow_origins`](crate::mcp::Server::allow_origins); a browser client also needs CORS headers and preflights, which CloudFront can answer.
- **No schema validation.** The handler's type is the check for hand-written tools; OpenAPI tools check that required arguments are present and leave the rest to the API. There is no `outputSchema`, no `x-mcp-header` parameter and no `tools/list` pagination.
- **OpenAPI, narrowly.** JSON documents of version 3 only; JSON request bodies only; no cookie parameters and no argument for a header the transport sets itself (`Accept`, `Content-Type`, `Authorization`, `Host`, `Content-Length`, `Transfer-Encoding`); local `$ref`s only; the document's `servers` are ignored in favour of the base URL you pass, and every path must start with `/`, so a call can only ever reach that base URL's host.
- **No authorization server.** It verifies tokens and publishes the resource metadata; it does not issue tokens, register clients or publish authorization-server metadata.
- **No scope step-up.** A `403` from the policy carries no `insufficient_scope` challenge, so a client cannot learn from it which scopes to ask for.
