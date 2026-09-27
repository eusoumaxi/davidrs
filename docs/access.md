# Access control and rate limits

Two stages of the HTTP pipeline decide whether a request may reach its handler: **admission** counts it before anything is parsed, and the **policy** decides who is calling and what they may do. Both are traits you can implement yourself ([`Admission`](crate::http::Admission), [`Policy`](crate::http::Policy)), and both come with a configurable implementation that covers most APIs without writing one: [`RateLimited`](crate::http::RateLimited) and [`Access`](crate::http::access::Access).

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

### The order of checks

A request with several problems always gets the earliest refusal:

1. a token that is present but invalid → `401` `ERROR_INVALID_TOKEN`
2. no caller on a route that requires one → `401` `ERROR_UNAUTHENTICATED`
3. a tenant the caller does not belong to, or an anonymous request naming one on a tenancy that is not public → `403` `ERROR_FORBIDDEN`
4. no tenant on a route that requires one → `400` `ERROR_TENANT_REQUIRED`
5. the permission rule says no → `403` `ERROR_FORBIDDEN`

Keep your own codes with [`Access::errors`](crate::http::access::Access::errors) and an [`AccessErrors`](crate::http::access::AccessErrors) value.

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
