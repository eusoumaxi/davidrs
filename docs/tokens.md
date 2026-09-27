# Bearer tokens

[`Verifier`](crate::auth::Verifier) checks a JSON Web Token signed with RS256 against the key set (JWKS) its issuer publishes. A [`VerifierConfig`](crate::auth::VerifierConfig) says what an acceptable token is: the exact issuer, the accepted audiences, claims that must equal a fixed value, the clock leeway for `exp`, and how often the key set may be fetched again. A successful check returns [`VerifiedClaims`](crate::auth::VerifiedClaims); a failed one returns a [`VerifyError`](crate::auth::VerifyError). The verifier authenticates and stops there: who may do what is your [`Policy`](crate::http::Policy).

Behind an API Gateway authorizer, the token is already verified before the function runs, and [`Access`](crate::http::access::Access) reads its claims: prefer that wherever a gateway can sit in front. The verifier is for Function URLs, MCP servers and direct invocations, and for a second check behind a gateway; the [security chapter](crate::guide::aws_security) compares the two.

## Why it exists

Hand-written token checks tend to fail open in a few known ways:

- **Trusting `alg`.** The header names the algorithm and the attacker writes the header. `none` skips the signature; `HS256` makes a naive verifier use the public key as an HMAC secret. Here anything but `RS256` is refused before a key is looked up.
- **A fetch per unknown `kid`.** Refreshing the key set when a token names an unknown key is how key rotation is picked up, but the `kid` is also attacker-controlled. Here concurrent misses share one fetch, and no fetch happens again until [`min_refresh_interval`](crate::auth::VerifierConfig::min_refresh_interval) has passed since the last successful one — or, after a failed fetch, a short cooldown of at most five seconds. A flood of forged tokens costs the identity provider at most one request per interval, even while it is down.
- **Unbounded input.** A token over [`MAX_TOKEN_BYTES`](crate::auth::MAX_TOKEN_BYTES) (8 KiB) is refused before it is decoded. A key set over [`MAX_JWKS_BYTES`](crate::auth::MAX_JWKS_BYTES) (64 KiB) is refused while it is read, and at most [`MAX_KEYS`](crate::auth::MAX_KEYS) RSA keys are kept from it. Keys of other types are skipped.
- **Overflowing `exp`.** Times are compared as floating-point seconds, so an absurd `exp` reads as far future or long past and never wraps into the opposite verdict.
- **Any audience.** A token issued to another application of the same issuer is refused: a verifier accepts no token until its audiences are set, and matches `client_id` when a token has no `aud`, as an API Gateway JWT authorizer does.
- **Telling the caller why.** `VerifyError` says which check failed so that you can log it. Answer every variant with the same `401`: a precise reason helps a forger more than a client.
- **Forged identity.** `VerifiedClaims` has no public constructor and does not implement `Deserialize`, so holding one proves a verification succeeded.

## How to use it

### Build the verifier once

Build it in `main` and keep it for the life of the process: the key cache lives in the verifier, so a warm invocation verifies without any request.

```rust,no_run
use davidrs::auth::{Verifier, VerifierConfig};
use davidrs::client::{self, Limits};

# async fn run() -> Result<(), davidrs::RuntimeError> {
let config = VerifierConfig::new(
    "https://id.example.com",
    "https://id.example.com/.well-known/jwks.json",
)
.with_audiences(vec!["web".to_owned(), "mobile".to_owned()])
.requiring("token_use", "id");
let verifier = Verifier::load(client::build(Limits::default())?, config).await?;
# let _ = verifier;
# Ok(())
# }
```

[`Verifier::load`](crate::auth::Verifier::load) fetches the key set before it returns, so an unreachable endpoint or a document with no usable RSA key fails the cold start, where a wrong URL is noticed at once. [`Verifier::deferred`](crate::auth::Verifier::deferred) returns immediately and fetches on the first verification instead; use it when many invocations never see a token, such as a function whose routes are mostly public.

The leeway defaults to 60 s. Set the public [`leeway`](crate::auth::VerifierConfig::leeway) field to change it.

### Turn it into a policy

A [`Policy`](crate::http::Policy) reads the `Authorization` header, verifies the token and maps the claims into the scope your handlers receive. Every failure is the same `401`; the reason goes into the failure's internal detail, which is logged and never rendered.

```rust,no_run
use std::sync::Arc;

use davidrs::auth::{bearer, Verifier, VerifierConfig};
use davidrs::client::{self, Limits};
use davidrs::http::{Api, Failure, Json, PlainErrors, Policy, Request, StatusCode};
use davidrs::{Context, Invocation, RuntimeError};

/// The signed-in user. Only [`SignedIn`] creates one, so a handler that
/// receives it knows the token was valid.
struct User {
    id: String,
}

/// Requires a valid bearer token.
struct SignedIn {
    verifier: Verifier,
}

fn unauthorized() -> Failure {
    Failure::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Sign in to continue")
}

impl Policy for SignedIn {
    type Scope = User;

    async fn authorize(&self, request: &Request<'_>, _: &Invocation) -> Result<User, Failure> {
        let header = request.header("authorization").ok_or_else(unauthorized)?;
        let claims = self
            .verifier
            .verify(bearer(header))
            .await
            .map_err(|error| unauthorized().with_detail(error.to_string()))?;
        let id = claims.subject().ok_or_else(unauthorized)?.to_owned();
        Ok(User { id })
    }
}

async fn whoami(_: Arc<()>, _: (), context: Context<User>) -> Result<Json<String>, Failure> {
    Ok(Json(context.scope().id.clone()))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let config = VerifierConfig::new(
        davidrs::required_env("TOKEN_ISSUER")?,
        davidrs::required_env("TOKEN_JWKS_URL")?,
    )
    .with_audiences(davidrs::list_env("TOKEN_AUDIENCES")?);
    let verifier = Verifier::load(client::build(Limits::default())?, config).await?;
    Api::new("whoami", SignedIn { verifier }, PlainErrors)
        .run(Arc::new(()), |_| Ok(()), whoami)
        .await
}
```

[`bearer`](crate::auth::bearer) strips the `Bearer ` prefix and surrounding whitespace. [`VerifiedClaims`](crate::auth::VerifiedClaims) reads the rest: [`subject`](crate::auth::VerifiedClaims::subject), [`expires_at`](crate::auth::VerifiedClaims::expires_at), [`string`](crate::auth::VerifiedClaims::string) and [`get`](crate::auth::VerifiedClaims::get) for one claim (a `null` claim reads as absent), and [`all`](crate::auth::VerifiedClaims::all) for the whole map.

```rust
use davidrs::auth::{bearer, VerifyError};

assert_eq!(bearer("Bearer eyJ.eyJ.c2ln "), "eyJ.eyJ.c2ln");
assert_eq!(VerifyError::UnknownKey.to_string(), "no key matched the token");
```

## Use cases

- A Function URL or a streamed response, where there is no API Gateway authorizer in front of the function.
- Handlers that need the claims themselves, such as the subject or a group claim, to scope a query to the caller.
- One function that accepts tokens from a single issuer for several client applications, each with its own audience.
- Picking up a rotated signing key without a deploy: the first token signed with the new `kid` refreshes the key set.

## What it does not do

- **RS256 only.** `ES256`, `PS256`, `EdDSA` and HMAC tokens are refused.
- **One issuer per verifier.** Build one verifier per issuer you accept.
- **`exp` and `nbf`, not `iat`.** `exp` is required and `nbf` is honoured when present, both with the configured leeway and both as `NumericDate`s that may have a fraction. `iat` is not checked; read it from `VerifiedClaims` if your issuer relies on it.
- **No revocation.** A token stays valid until it expires. A key removed from the key set stays trusted until an unknown `kid` causes the next refresh. Amazon Cognito documents the same for its revoked tokens — they still verify by signature and expiry — so keep access tokens short-lived.
- **No background refresh.** The key set is fetched only by `load` and by an unknown `kid`, never on a timer.
- **No authorization.** Roles, groups, tenants and permissions are claims the verifier does not interpret. Map them into your policy's scope.
