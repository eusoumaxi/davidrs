# Access control and rate limits

Two stages of the HTTP pipeline decide whether a request reaches its handler. **Admission** counts it before anything is parsed. The **policy** decides who is calling and what they may do. Both are traits you can implement ([`Admission`](crate::http::Admission), [`Policy`](crate::http::Policy)). Most APIs do not need to: [`RateLimited`](crate::http::RateLimited) and [`Access`](crate::http::access::Access) cover them from configuration. Enable `http`. Add `auth` only when the function itself verifies the bearer token. Add `dynamo` when the counter is [`DynamoWindow`](crate::http::rate_limit::DynamoWindow).

Put identity and permission in the policy, after the body has been decoded. The handler then receives a [`Grant`](crate::http::access::Grant) whose type already says whether a caller and a tenant are present, and it should not read the `Authorization` header again. Put a count that must refuse the request before parsing in admission. Floods and abusive addresses should not reach either stage: AWS WAF and API Gateway throttling refuse them before the function is invoked. `RateLimited` is for a quota those cannot express, such as 1,000 exports per verified user per day. [Security on AWS](crate::guide::aws_security) says which product does which of those jobs.

## Who is calling: `Access`

### What it is

A [`Policy`](crate::http::Policy) built from configuration. It identifies the caller from claims an API Gateway authorizer already verified, or from a bearer token it verifies itself; requires a caller or not; selects the tenant a multi-tenant request acts for; applies your permission rule; and refuses with codes you choose. The handler receives a [`Grant`](crate::http::access::Grant) typed by what the route requires.

### Why it exists

Every API answers the same questions before its handler runs, and the answers are easy to get subtly wrong: trusting a tenant header without checking membership, treating a malformed token as "anonymous", answering `403` where the client needed `401`, or unwrapping an optional caller the route promised was there. `Access` answers them once, in a fixed order, and makes the guarantees part of the handler's types.

### How to use it

Map claims to your caller type, then state what the route requires:

```rust
use std::sync::Arc;

use davidrs::http::access::{Access, Claims, Grant, Tenancy};
use davidrs::http::{Api, Failure, Json, PlainErrors, Request};
use davidrs::Context;

/// The application's caller.
struct User {
    id: String,
    tenants: Vec<String>,
    roles: Vec<String>,
}

/// Claims become a user when they carry a subject.
fn user(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.subject()?.to_owned(),
        tenants: claims.list("tenants"),
        roles: claims.list("cognito:groups"),
    })
}

/// A route for members of a tenant who hold the `billing` role.
fn billing_policy() -> Access<User, User, String> {
    Access::new(user)
        .require_caller()
        .tenancy(Tenancy::header("x-tenant-id", |user: &User, tenant| {
            user.tenants.iter().any(|own| own == tenant)
        }))
        .require_tenant()
        .permit(|user, _tenant| user.is_some_and(|user| user.roles.iter().any(|role| role == "billing")))
}

/// Nothing to unwrap: the policy guaranteed a user and a tenant.
async fn invoices(_app: Arc<()>, _input: (), context: Context<Grant<User, String>>) -> Result<Json<String>, Failure> {
    let grant = context.scope();
    Ok(Json(format!("invoices of {} for {}", grant.tenant(), grant.caller().id)))
}

let api = Api::new("list-invoices", billing_policy(), PlainErrors);
# let _ = (api, |request: &Request| Ok::<_, Failure>(()), invoices);
```

The builder changes the grant's types as it goes:

| Configuration       | The handler's grant        |
| ------------------- | -------------------------- |
| `Access::new(user)` | `Grant<Option<User>, ()>`  |
| `.require_caller()` | `Grant<User, ()>`          |
| `.tenancy(..)`      | `Grant<_, Option<String>>` |
| `.require_tenant()` | `Grant<_, String>`         |

### Where the caller comes from

1. **An API Gateway authorizer** (on by default). The HTTP API JWT authorizer's claims — every value a string, arrays written `"[a b]"` — a Lambda authorizer's context, or, with `apigw-rest`, a REST API Cognito authorizer's `claims`. [`Claims::list`](crate::http::access::Claims::list) reads every list shape, so one mapping serves all of them.
2. **A bearer token the function verifies** (feature `auth`), for Function URLs and routes without an authorizer: [`Access::verify_bearer`](crate::http::access::Access) with an [`auth::Verifier`](crate::auth::Verifier) for your issuer's JWKS — Amazon Cognito, Auth0, Okta, Entra ID or any RS256 OpenID Connect provider. See [tokens](crate::guide::tokens).

Prefer the gateway authorizer wherever one can sit in front: it refuses bad tokens before the function is invoked. `verify_bearer` is for Function URLs, MCP servers and direct invocations, and as a second check behind a gateway; the [security chapter](crate::guide::aws_security) compares the two.

Turn gateway claims off with `.gateway_claims(false)` when anything other than API Gateway may invoke the function.

### Tokens the gateway already verified

API Gateway JWT and Cognito authorizers validate tokens before invoking the function, according to the authorizer and route configuration. Configure the intended issuer, audience and required scopes. A Lambda authorizer implements its own checks: its code must establish the identity it returns. `Access` maps the resulting claims into your caller type; it does not add missing authorizer checks.

- **Use the verified context.** When the gateway is the only permitted invoker and its authorizer enforces your token requirements, `Access` can identify the caller without `verify_bearer`. Independently verify tokens when that trust boundary does not hold.
- **Map the claims.** `Access` hands the authorizer's claims to your mapping function. An HTTP API authorizer passes them as strings, and a list arrives as `"[a b]"`, which [`Claims::list`](crate::http::access::Claims::list) reads.
- **Keep nested claims whole** with [`gateway_token_claims(true)`](crate::http::access::Access::gateway_token_claims) only when the authorizer verifies the exact bearer token in `Authorization` and the integration preserves that header. This reads its payload without checking the signature. Matching `iss` and shared string claims are consistency checks, not proof of verification: a cookie, query parameter or different authorization header must not identify the caller in this mode. Use the gateway claims or `verify_bearer` instead when that condition cannot be guaranteed. [`Claims::from_gateway_token`](crate::http::access::Claims::from_gateway_token) has the same requirements.
- **Restrict who may invoke.** Gateway claims are trusted because only API Gateway writes them: grant `lambda:InvokeFunction` to `apigateway.amazonaws.com` for that API alone.

How common providers look behind an HTTP API JWT authorizer, whose configuration is an issuer and a list of audiences:

| Provider | Issuer | Audience the authorizer checks | Claims to map |
| --- | --- | --- | --- |
| Amazon Cognito | `https://cognito-idp.<region>.amazonaws.com/<pool-id>` | the app client ID, matched against an access token's `client_id` | `sub`, `scope` (space-separated), `cognito:groups` (a list) |
| Auth0 | your Auth0 domain with a trailing slash, `https://<tenant>.auth0.com/` | the API identifier in `aud` | `sub`, `scope`, `permissions` (a list, with RBAC), namespaced claims such as `https://example.com/roles` |
| Okta | a custom authorization server, `https://<org>.okta.com/oauth2/<server-id>` | the server's audience | `sub`, `scp` (a list), `groups` when a claim is configured |
| Microsoft Entra ID | `https://login.microsoftonline.com/<tenant-id>/v2.0` | the API's application (client) ID | `oid` (a stable user ID), `scp` (space-separated), `roles` (a list) |
| Keycloak | `https://<host>/realms/<realm>` | the audience an audience mapper adds for the API | `sub`, `realm_access.roles` (nested) |
| Clerk | your Clerk Frontend API URL | none by default: a session token has no `aud` | a JWT template that adds `aud` and the claims you need |

One mapping per provider, all built on the same caller type:

```rust
use davidrs::http::access::{Access, Claims};
use serde_json::Value;

/// The application's caller, whichever provider signed the token.
struct User {
    id: String,
    roles: Vec<String>,
    scopes: Vec<String>,
}

/// An Amazon Cognito access token.
fn cognito(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.subject()?.to_owned(),
        roles: claims.list("cognito:groups"),
        scopes: claims.list("scope"),
    })
}

/// An Auth0 access token with RBAC permissions and a namespaced roles claim.
fn auth0(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.subject()?.to_owned(),
        roles: claims.list("https://example.com/roles"),
        scopes: claims.list("permissions"),
    })
}

/// A Microsoft Entra ID access token, whose stable user ID is `oid`.
fn entra(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.string("oid")?.to_owned(),
        roles: claims.list("roles"),
        scopes: claims.list("scp"),
    })
}

/// A Keycloak access token, whose realm roles sit in a nested object.
fn keycloak(claims: &Claims) -> Option<User> {
    let roles = claims
        .get("realm_access")
        .and_then(|access| access.get("roles"))
        .and_then(Value::as_array)
        .map(|roles| roles.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    Some(User {
        id: claims.subject()?.to_owned(),
        roles,
        scopes: claims.list("scope"),
    })
}

let cognito_policy = Access::new(cognito).require_caller();
let auth0_policy = Access::new(auth0).require_caller();
let entra_policy = Access::new(entra).require_caller();
let keycloak_policy = Access::new(keycloak).gateway_token_claims(true).require_caller();
# let _ = (auth0_policy, entra_policy, keycloak_policy, cognito_policy);

let verified = Claims::new(
    serde_json::from_value(serde_json::json!({
        "sub": "alice",
        "cognito:groups": "[admin editors]",
        "scope": "orders/read orders/write",
    }))
    .unwrap(),
);
let user = cognito(&verified).unwrap();
assert_eq!(user.roles, ["admin", "editors"]);
assert_eq!(user.scopes, ["orders/read", "orders/write"]);
```

**Opaque tokens.** An OAuth access token that is not a JWT — a reference token, as GitHub and some authorization servers issue — cannot be read or verified inside the function: the JWT authorizer refuses it, and [`auth::Verifier`](crate::auth::Verifier) reads RS256 JWTs only. Put a Lambda authorizer in front that asks the authorization server about the token, through its introspection endpoint (RFC 7662) or its user endpoint, and returns the caller in its context, such as `{"isAuthorized": true, "context": {"sub": "…", "scope": "…"}}`. API Gateway caches the answer per token, and `Access` reads that context like any other gateway claims, so a mapping such as `cognito` above works unchanged.

**No gateway in front.** A Function URL, an MCP server behind CloudFront or a direct invocation has no authorizer, and its token is only a claim until verified: use `verify_bearer`, described in [tokens](crate::guide::tokens).

### The order of checks

A request with several problems always gets the earliest refusal:

1. a token that is present but invalid → `401` `ERROR_INVALID_TOKEN`
2. no caller on a route that requires one → `401` `ERROR_UNAUTHENTICATED`
3. a tenant the caller does not belong to, or an anonymous request naming one on a tenancy that is not public → `403` `ERROR_FORBIDDEN`
4. no tenant on a route that requires one → `400` `ERROR_TENANT_REQUIRED`
5. the permission rule says no → `403` `ERROR_FORBIDDEN`

Keep your own codes with [`Access::refusals`](crate::http::access::Access::refusals) and a [`Refusals`](crate::http::access::Refusals) value.

### Use cases

- A SaaS API where each user belongs to several workspaces and picks one per request with a header.
- A public catalogue that shows more to signed-in users: an optional caller, no tenancy.
- An admin endpoint: a required caller and a permission rule on a group claim.
- A Function URL with no gateway: bearer verification in the function, with the protections the [security chapter](crate::guide::aws_security) lists for Function URLs.

### What it does not do

- It does not issue tokens, refresh them or manage sessions: that is your identity provider's job.
- It does not load permissions from a database. When a rule needs data, do the lookup in the handler, or implement [`Policy`](crate::http::Policy) yourself — `Access` is a convenience, not the only way.
- A tenant in a grant is membership. An anonymous request that names a tenant is refused, unless the tenancy is [`public`](crate::http::access::Tenancy::public) — a storefront or public catalogue — where the tenant is context, not membership.

## How much they may call: `RateLimited`

### What it is

An [`Admission`](crate::http::Admission) that enforces a **business quota**: it counts each request under a key — a verified user, a tenant, an API key, or the caller's address — with a [`Counter`](crate::http::Counter), refuses past the limit with `429`, `Retry-After` and the IETF `RateLimit-Policy` / `RateLimit` headers, and puts the remaining budget on every successful response too.

### Why it exists — and what to use first

Floods, abusive addresses and bots are the platform's job: an AWS WAF rate-based rule and API Gateway throttling refuse that traffic before the function is invoked, at no cost to it. Use them first; the [security chapter](crate::guide::aws_security) shows where they attach.

What a firewall cannot count is a quota that belongs to your product — 1,000 exports per user per day, 50 searches per tenant per minute on the free plan — because its keys are addresses and headers, never the verified identity behind a token. That is what `RateLimited` is for. Its counter lives outside the function, because a limit counted in memory only sees the requests one Lambda instance happened to receive, and its refusal tells the client exactly when to come back.

### How to use it

```rust,no_run
# #[cfg(feature = "dynamo")]
# async fn build(client: aws_sdk_dynamodb::Client) {
use std::time::Duration;

use davidrs::http::rate_limit::DynamoWindow;
use davidrs::http::{Api, PlainErrors, Public, RateLimitConfig, RateLimited, Request};

let limit = RateLimitConfig::new("search", 120, Duration::from_secs(60));
let api = Api::new("search", Public, PlainErrors).admission(
    RateLimited::new(DynamoWindow::new(client, "rate-limits"), limit)
        .key(|request: &Request| request.header("x-api-key").unwrap_or("anonymous").to_owned()),
);
# let _ = api;
# }
```

[`DynamoWindow`](crate::http::rate_limit::DynamoWindow) (feature `dynamo`) counts in one DynamoDB item per key and window, incremented atomically and expired by the table's TTL. Any other store — a cache cluster, a relational database — is a [`Counter`](crate::http::Counter) implementation of a few lines.

### Use cases

- Plan quotas per verified user or tenant, keyed with [`Claims::from_gateway`](crate::http::access::Claims::from_gateway) on what the authorizer verified.
- Partner APIs: a quota per API key or per tenant with [`RateLimited::key`](crate::http::RateLimited::key).
- Expensive operations (a search that fans out to upstream services): a tighter quota on that one function.
- A public operation that needs an exact quota, behind a WAF rate-based rule that already stops floods.

### What it does not do

- It is a fixed window: a caller can spend a window's budget at its end and the next one's at its start. For smooth limits, implement a sliding-window `Counter`.
- If the counter's store is unreachable, the request is refused with a `500`, never admitted uncounted.
- It is not flood protection: every counted request has already invoked the function. Stop floods with AWS WAF and API Gateway throttling, before the function runs.
- Streamed routes have no admission stage: a [`StreamApi`](crate::http::stream::StreamApi) handler calls its counter itself, once it knows who is calling.
