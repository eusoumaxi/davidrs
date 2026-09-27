# Security on AWS: the platform first

Let AWS enforce everything it can before your code runs. The recommended front door for a function built with `davidrs` is **Amazon API Gateway with AWS WAF and Amazon Cognito**, behind Amazon CloudFront where a custom domain, caching or a WAF on an HTTP API is needed. The gateway and the firewall verify tokens, throttle, rate-limit, inspect request shape, reputation and bots, and a request they reject never invokes the function. `davidrs` can verify tokens and count requests itself, and the crate still recommends the AWS services for those jobs: its own rate limiting exists for the **business quotas per operation, tenant or user** a firewall cannot express, and its token verification for the places where **no authorizer sits in front** (Function URLs, MCP servers, direct invocations) and as defense in depth everywhere else.

This chapter says, for each concern, what AWS does, what `davidrs` does, why the crate does it anyway, and what to configure. Every limit quoted here comes from the AWS documentation linked next to it; limits change, so check the link before you rely on a number.

## The principle

A control that runs in the platform is better than the same control in the function, for three reasons:

- **Rejected traffic costs nothing.** A request the gateway or the firewall refuses is never an invocation: no cold start, no execution time, no log line, no pressure on the function's concurrency or on the stores behind it. A flood that reaches the function is already a bill.
- **The controls are managed and audited.** Token signatures, throttling counters, IP reputation lists and bot signatures are maintained by AWS, configured as infrastructure, visible in CloudTrail and CloudWatch, and reviewed by whoever reviews the account, not only by whoever reviews the code.
- **They apply before anything is parsed.** A firewall rule inspects bytes; an authorizer inspects a header. Neither needs the function's decoder, so a malformed body, an oversized request or a forged token is refused without running the code that would have to handle it.

What remains for the function is what only the application knows: which tenant a caller belongs to, which operation a caller may perform on which record, how much of a business quota is left, what a valid body means, which upstream to call and with what budget. `davidrs` is built for that remainder. Its pipelines assume a hostile client can still reach them, so the bounds, the deadline, the fixed `5xx` message and the tenant check stay in place even behind a perfect gateway; they are the second layer, not a substitute for the first.

## The recommended architecture

```text
clients ──▶ CloudFront ──────▶ AWS WAF ──────▶ API Gateway ─────▶ Lambda ─────▶ AWS services
            TLS termination     rate rules      authorizer          davidrs        DynamoDB, SQS,
            Shield Standard     managed rules   (Cognito / JWT)     pipeline       EventBridge,
            caching, custom     bot control     throttling          handler        Secrets Manager
            domain              body limits     request validation
                                                access logs
```

Read it as a series of filters. Each stage refuses what it can and hands the rest on; the function sees only what every stage before it accepted.

- **CloudFront** terminates TLS with a certificate from AWS Certificate Manager, absorbs network and transport layer floods with [AWS Shield Standard](https://aws.amazon.com/shield/features/) at no charge, caches what may be cached, and is the one place a web ACL can protect an HTTP API or a Function URL.
- **AWS WAF** inspects each request against rate-based rules and managed rule groups, and blocks, counts or challenges before the request reaches the gateway.
- **API Gateway** verifies the bearer token with an authorizer backed by Amazon Cognito (or any OpenID Connect issuer), applies throttling per stage, route or client, validates the request shape on a REST API, and writes access logs.
- **Lambda** runs the `davidrs` pipeline: admission, bounded decoding, policy, handler, renderer, under one deadline.

**Where a web ACL attaches.** AWS WAF associates with a CloudFront distribution (through the distribution's configuration) and, as a regional resource, with an Application Load Balancer, an **API Gateway REST API stage**, an AWS AppSync API, an Amazon Cognito user pool, an AWS App Runner service, an AWS Verified Access instance, an AWS Amplify app and an Amazon Bedrock AgentCore gateway ([AssociateWebACL](https://docs.aws.amazon.com/waf/latest/APIReference/API_AssociateWebACL.html)). **API Gateway HTTP APIs and Lambda Function URLs are not on that list**; the API Gateway comparison table says the same for HTTP APIs ([Choose between REST APIs and HTTP APIs](https://docs.aws.amazon.com/apigateway/latest/developerguide/http-api-vs-rest.html)). To put a firewall in front of either, put CloudFront in front of it and attach the web ACL to the distribution, then make sure the origin is not reachable without CloudFront: origin access control for a Function URL, and for an HTTP API a secret header that CloudFront adds and the function checks.

Two smaller arrangements are also fine, and the rest of this chapter says when:

```text
clients ──▶ API Gateway HTTP API (JWT authorizer, throttling) ──▶ Lambda        a signed-in API, no firewall
clients ──▶ CloudFront (WAF, OAC) ──▶ Function URL (RESPONSE_STREAM) ──▶ Lambda  a streamed or long response
```

## Who does what

One row per concern. "AWS" is what the platform offers with its real limits; "`davidrs`" is what the crate does; "why" is the gap the crate fills, or the reason to keep the control in the function even when the platform has one.

| Concern | AWS offers | `davidrs` does | Why the crate does it | Recommendation |
| --- | --- | --- | --- | --- |
| TLS termination | CloudFront and API Gateway custom domains with ACM certificates and TLS security policies; a Function URL is HTTPS only on its `lambda-url.<region>.on.aws` name | Nothing inbound. Outbound, [`client::build`](crate::client::build) speaks TLS 1.2 and 1.3 with the Mozilla roots compiled in | The function never sees a TLS handshake | Platform only |
| DDoS | Shield Standard on CloudFront and Route 53 (layers 3 and 4, always on); the WAF Anti-DDoS managed rule group (layer 7); API Gateway throttling | Nothing | A flood must be absorbed before an invocation exists | CloudFront in front, WAF, throttling |
| IP reputation, anonymous IPs, bots | WAF managed rule groups: Amazon IP reputation list, anonymous IP list, Bot Control | Nothing | No function can know what the edge knows | WAF; CloudFront in front of HTTP APIs and Function URLs |
| Rate limiting by IP or by key | WAF rate-based rules: 10 requests or more per 60, 120, 300 or 600 s window, approximate, keyed on IP, forwarded IP, header, cookie, query, path, method, JA3/JA4, ASN or labels. API Gateway throttling: 10,000 requests per second with a 5,000 burst per account and Region, overridable per stage, route or method. Function URLs: reserved concurrency only | [`RateLimited`](crate::http::RateLimited): an exact fixed window under any key, with `RateLimit-Policy`, `RateLimit` and `Retry-After` on every response | WAF and throttling stop floods, but cannot count per verified user, tenant or operation, cannot use a token claim as a key, and answer no budget headers | Floods at the edge; business quotas in the function |
| Quotas and API keys | REST API usage plans: per-key rate, burst and a daily, weekly or monthly quota. HTTP APIs and Function URLs have no keys or plans | [`RateLimited::key`](crate::http::RateLimited::key) counts under an API key, a tenant or a user | Partner quotas without a REST API, and quotas keyed on what the authorizer verified rather than on a header | Usage plans for partner keys on a REST API; `RateLimited` otherwise |
| Authentication (JWT validation) | HTTP API JWT authorizer, REST API Cognito authorizer, Lambda authorizers cached 1 to 3,600 s | [`Access`](crate::http::access::Access) reads the authorizer's claims; [`Access::verify_bearer`](crate::http::access::Access::verify_bearer) with a [`Verifier`](crate::auth::Verifier) verifies RS256 tokens itself | Function URLs, MCP servers and direct invocations have no authorizer; verifying again behind one is defense in depth | The authorizer; `verify_bearer` only where there is none |
| Authorization by scopes and groups | `authorizationScopes` on an HTTP API route, OAuth scopes on a REST API method: the token must carry one of them. Groups are not evaluated by the gateway | [`Access::permit`](crate::http::access::Access::permit) over any claim, such as `cognito:groups` | The gateway checks scopes; roles, groups and rules that mix claim and record are the function's | Scopes at the gateway, the rule in `permit` |
| Tenant membership | Nothing decides membership. Lambda tenant isolation mode gives each tenant id its own execution environments, which is isolation, not authorization | [`Tenancy`](crate::http::access::Tenancy) and [`Grant`](crate::http::access::Grant) | Only the application knows who belongs where | Always in the function |
| Request size | API Gateway payload 10 MB; Lambda 6 MB per synchronous request and response; WAF inspects the first 16 KB of a body on CloudFront and API Gateway (8 KB on an Application Load Balancer) | [`Api::body_limit`](crate::http::Api::body_limit), 1 MiB by default, checked before any decoder; bounds on tokens, key sets, upstream answers and MCP bodies | 10 MB is a platform limit, not a business one; a route knows what it needs | Set `body_limit` per route |
| Request schema validation | REST API request validators: a JSON Schema model for the body, required parameters and headers. HTTP APIs: none | Typed decoding with serde, [Garde validation](crate::guide::validation), [`http::schema`](crate::http::schema) collecting every invalid field | The handler's type is the schema; a validator model would be a second copy of it | Optional on a REST API for a cheap `400`; the function decodes regardless |
| CORS | HTTP API CORS configuration (the gateway answers preflights and ignores the integration's CORS headers); REST API proxy integrations must return the headers themselves and answer `OPTIONS` with a mock integration; Function URL CORS configuration | [`Cors`](crate::http::stream::Cors) on a streamed route, [`Server::allow_origins`](crate::mcp::Server::allow_origins) on an MCP server, [`Api::finalize`](crate::http::Api::finalize) on a buffered one | Exactly one layer must own the headers; the crate offers one where the platform's layer is missing or approximate | The gateway or the URL configuration for buffered routes; `Cors` for streamed ones |
| Error redaction | Gateway responses for what the gateway itself refuses, customizable on a REST API; an integration failure becomes `{"message": "Internal server error"}` | [`Failure::public_message`](crate::http::Failure::public_message) is a fixed string for every `5xx` | The gateway cannot redact a body the function wrote | Both |
| Timeouts and deadlines | HTTP API integration timeout 30 s; REST API 29 s, raisable for Regional and private APIs, and up to 15 minutes in `STREAM` mode; CloudFront origin response timeout 30 s by default, 1 to 120 s; a Function URL runs to the function's timeout, at most 15 minutes | One [`Deadline`](crate::Deadline) per invocation with a margin, and a rendered `504` `ERROR_TIMEOUT` | A platform timeout cuts the connection with no body from the function; the pipeline answers first | Function timeout below the gateway's; the pipeline renders the `504` |
| SQS partial batch failures | `ReportBatchItemFailures` on the event source mapping, a dead-letter queue and a redrive policy | [`queue::run`](crate::queue::run) reports a [`Disposition`](crate::queue::Disposition) per record | Lambda honours the report only when configured; the function must produce an honest one | Both |
| Secrets | Secrets Manager with one IAM policy per function; the AWS Parameters and Secrets Lambda Extension caches values in the sandbox | [`secrets`](crate::secrets) reads a typed value once and never echoes it | Reading is the function's job; the crate keeps values out of errors and logs | Secrets Manager, read in `main`; never environment variables |
| Tracing | X-Ray active tracing on the function and on a REST API stage; HTTP APIs have no X-Ray | The `otel` feature sends spans to the X-Ray agent in the sandbox | In `PassThrough` mode a function behind an HTTP API, a Function URL or SQS is never sampled | Active tracing plus `otel` |

## Lambda Function URLs, explained

A Function URL is a dedicated HTTPS endpoint on the function itself, `https://<url-id>.lambda-url.<region>.on.aws`, with nothing between the client and Lambda ([Invoking Lambda function URLs](https://docs.aws.amazon.com/lambda/latest/dg/urls-invocation.html)).

**Auth types.** `AWS_IAM` requires every request to be signed with Signature Version 4 by a principal allowed `lambda:InvokeFunctionUrl` and `lambda:InvokeFunction`. `NONE` performs no authentication, and still needs a resource-based policy that grants both actions to `*`, with the conditions `lambda:FunctionUrlAuthType` = `NONE` and `lambda:InvokedViaFunctionUrl` = `true` so the statement opens only the URL and not the `Invoke` API; the console and AWS SAM write that policy, the CLI, CloudFormation and the API do not ([Control access to Lambda function URLs](https://docs.aws.amazon.com/lambda/latest/dg/urls-auth.html)).

**Invoke modes.** `BUFFERED` invokes with `Invoke` and answers when the payload is complete, at most 6 MB. `RESPONSE_STREAM` invokes with `InvokeWithResponseStream`: up to 200 MB per response, uncapped bandwidth for the first 6 MB and 2 MB/s after it ([Lambda quotas](https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html), [response streaming](https://docs.aws.amazon.com/lambda/latest/dg/configuration-response-streaming.html)).

**Payload.** The event is API Gateway payload format 2.0: `rawPath`, `rawQueryString`, `headers`, `cookies`, `body` with `isBase64Encoded`, and `requestContext.http.sourceIp`, which is the address of the immediate TCP connection: behind CloudFront that is CloudFront's address. With `AWS_IAM`, `requestContext.authorizer.iam` names the signer. [`Api`](crate::http::Api) and [`StreamApi`](crate::http::stream::StreamApi) read this payload without any configuration.

**CORS.** A URL has its own CORS configuration (`AllowOrigins`, `AllowMethods`, `AllowHeaders`, `ExposeHeaders`, `AllowCredentials`, `MaxAge`, whose default is `0`). Lambda answers preflights from it and adds the headers to every response. If the function also writes CORS headers, non-preflight responses carry both sets and browsers reject the duplicate, so pick one owner: the URL configuration, or [`Cors`](crate::http::stream::Cors) with the URL's CORS left empty ([Creating and managing Lambda function URLs](https://docs.aws.amazon.com/lambda/latest/dg/urls-configuration.html)).

**Limits.** 6 MB per request and buffered response, 1 MB for the request line and headers together, and the function's own timeout, at most 15 minutes: nothing shorter sits in the path unless CloudFront does.

**What a Function URL lacks.** No authorizers, no web ACL, no throttling, no usage plans, no request validation, no access logs of its own. The only brake is reserved concurrency: a function admits at most ten requests per second per unit of reserved concurrency and answers `429` beyond that ([Throttling function URLs](https://docs.aws.amazon.com/lambda/latest/dg/urls-configuration.html)). Reserved concurrency is therefore a hard cap on the bill, not a rate limit per caller.

**How to protect one.** Put CloudFront in front, and make the URL unreachable except through it:

1. Set the URL's auth type to `AWS_IAM` and create an origin access control of type `lambda` that always signs origin requests. Grant the CloudFront service principal `lambda:InvokeFunctionUrl` and `lambda:InvokeFunction` on the function, with `AWS:SourceArn` equal to the distribution's ARN, so only that distribution may invoke ([Restrict access to a Lambda function URL origin](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-restricting-access-to-lambda.html)).
2. **`POST` and `PUT` through OAC need the body's hash.** Lambda refuses unsigned payloads, so the viewer must send `x-amz-content-sha256` with the SHA-256 of the body; a browser client computes it before the request, and a route that lists that header in [`Cors::allow_headers`](crate::http::stream::Cors::allow_headers) lets the preflight through. Clients that cannot add the header, such as MCP clients and third-party webhook senders, cannot use OAC: the URL then stays `NONE` and the function is the gate.
3. **OAC signs in `Authorization`.** The `no-override` signing option forwards a viewer's `Authorization` header, but Lambda then validates it as a SigV4 signature for the URL's host, so a bearer token cannot be carried there. Have a CloudFront Function copy the token into another header on the viewer request, and restore it with [`StreamApi::prepare`](crate::http::stream::StreamApi::prepare) or a decoder before the policy reads it.
4. Attach a web ACL to the distribution, cache nothing on the API behaviour, and forward the headers the function reads with an origin request policy such as the managed `AllViewerExceptHostHeader` ([managed origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-origin-request-policies.html)).
5. Set reserved concurrency to the most instances the operation should ever run, and CloudFront's origin response timeout to what a response really needs: 30 s by default, 1 to 120 s per origin ([`OriginReadTimeout`](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-properties-cloudfront-distribution-customoriginconfig.html)); a streamed response that runs longer is cut by CloudFront, not by Lambda.

Behind CloudFront, the request's `sourceIp` is CloudFront's. Rate-limit on the viewer's address instead, which CloudFront writes in `CloudFront-Viewer-Address` as `ip:port` when the origin request policy forwards it ([CloudFront request headers](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/adding-cloudfront-headers.html)). A viewer cannot set that header on a URL that is only reachable through OAC; on a URL that is still reachable directly, it can, which is one more reason to close the direct path.

```rust
use davidrs::http::Request;

/// The viewer's address as CloudFront wrote it, without the port. Trust it
/// only on a URL that is reachable through the distribution alone.
fn viewer_address(request: &Request<'_>) -> String {
    request
        .header("cloudfront-viewer-address")
        .and_then(|address| address.rsplit_once(':'))
        .map(|(ip, _port)| ip.to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}
# let _ = viewer_address;
```

**When a Function URL is the right choice.** A streamed response ([`StreamApi`](crate::http::stream::StreamApi) in `RESPONSE_STREAM` mode); a response that needs more than the 30 s an HTTP API allows; a webhook whose sender only knows how to `POST` to a URL; an MCP server, whose clients send bearer tokens and cannot sign requests; an internal call from another AWS principal with `AWS_IAM`. For a signed-in API with many clients, choose API Gateway: authorizers, throttling, keys, validation and a web ACL of its own are what the URL does not have.

**Where `davidrs` fits.** With auth type `NONE` the function is the only gate, so the policy verifies the bearer token itself: [`Access::verify_bearer`](crate::http::access::Access::verify_bearer) with [`gateway_claims(false)`](crate::http::access::Access::gateway_claims), because nothing in front could have written trustworthy claims. Count requests with [`RateLimited`](crate::http::RateLimited) under a key the caller cannot choose. Serve MCP with [`mcp::Server`](crate::mcp::Server) exactly this way.

```rust
use std::sync::Arc;

use davidrs::auth::{Verifier, VerifierConfig};
use davidrs::client::{self, Limits};
use davidrs::http::access::{Access, Claims};

/// Whoever the token is about.
fn subject(claims: &Claims) -> Option<String> {
    claims.subject().map(str::to_owned)
}

# fn main() -> Result<(), davidrs::RuntimeError> {
let issuer = "https://cognito-idp.us-east-1.amazonaws.com/us-east-1_EXAMPLE";
let tokens = VerifierConfig::new(issuer, format!("{issuer}/.well-known/jwks.json"))
    .with_audiences(vec!["https://api.example.com".to_owned()])
    .requiring("token_use", "access");
let verifier = Arc::new(Verifier::deferred(client::build(Limits::default())?, tokens));

let policy = Access::new(subject)
    .gateway_claims(false)
    .verify_bearer(verifier)
    .require_caller();
# let _ = policy;
# Ok(())
# }
```

[`Verifier::deferred`](crate::auth::Verifier::deferred) fetches the key set on the first token; [`Verifier::load`](crate::auth::Verifier::load) fetches it at cold start so a wrong URL fails the deploy instead of the first user. The [tokens chapter](crate::guide::tokens) covers what the verifier checks and what it refuses.

## API Gateway, explained

API Gateway offers two products for HTTP ([Choose between REST APIs and HTTP APIs](https://docs.aws.amazon.com/apigateway/latest/developerguide/http-api-vs-rest.html)). HTTP APIs are smaller, cheaper and enough for most functions; REST APIs add the management features. Both read the same [`Api`](crate::http::Api); REST payloads need the `apigw-rest` feature.

| Capability | HTTP API | REST API |
| --- | --- | --- |
| Authorizers | JWT authorizer, Lambda authorizer, IAM | Cognito user pool authorizer, Lambda authorizer, IAM |
| Throttling | Stage and route level; account limit shared | Stage and method level; usage plans with API keys, per-client rate, burst and quota |
| Request validation | No | JSON Schema models for the body, required parameters and headers |
| Payload | 10 MB | 10 MB; more in `STREAM` mode |
| Integration timeout | 30 s, not raisable | 29 s by default; raisable for Regional and private APIs at the cost of throttle quota; up to 15 minutes in `STREAM` mode |
| Response streaming | No | `STREAM` response transfer mode on a Lambda proxy integration |
| Mutual TLS | Yes | Yes |
| Resource policies, private endpoints | No | Yes |
| AWS WAF | No; CloudFront in front | Yes, on the stage |
| CORS | Configured on the API; the gateway answers preflights and replaces the integration's headers | The integration returns the headers; `OPTIONS` through a mock integration |
| Access logs | CloudWatch Logs | CloudWatch Logs and Amazon Data Firehose; execution logs; X-Ray |

Sources: [HTTP API quotas](https://docs.aws.amazon.com/apigateway/latest/developerguide/http-api-quotas.html), [API Gateway quotas](https://docs.aws.amazon.com/apigateway/latest/developerguide/limits.html), [response transfer mode](https://docs.aws.amazon.com/apigateway/latest/developerguide/response-transfer-mode.html), [CORS for HTTP APIs](https://docs.aws.amazon.com/apigateway/latest/developerguide/http-api-cors.html).

**JWT authorizer (HTTP API).** For a route with the authorizer, the gateway reads the token from the identity source (`$request.header.Authorization`, with or without `Bearer`), checks the signature against the issuer's `jwks_uri` (RSA algorithms only, keys cached for up to two hours), and then `kid`, `iss` against the configured issuer, `aud` against the configured audiences (or `client_id` when `aud` is absent, which is the case of an Amazon Cognito access token), `exp`, `nbf`, `iat`, and `scope` or `scp` against the route's `authorizationScopes`, of which the token must hold at least one. Any failure is a `401` from the gateway, and the function is not invoked ([JWT authorizers](https://docs.aws.amazon.com/apigateway/latest/developerguide/http-api-jwt-authorizer.html)). Because nothing distinguishes an access token from an ID token, require scopes on every protected route, or an audience the issuer uses only for access tokens.

**Cognito user pool authorizer (REST API).** With no OAuth scopes on the method it accepts the pool's ID token and passes its claims; with scopes it requires an access token carrying one of them, in the form `resource-server-identifier/scope` ([Cognito user pools as an authorizer](https://docs.aws.amazon.com/apigateway/latest/developerguide/apigateway-integrate-with-cognito.html)).

**Lambda authorizers.** Your own function, invoked by the gateway. On a REST API a `TOKEN` authorizer receives one bearer token and a `REQUEST` authorizer receives headers, query and context, and both return an IAM policy; results are cached from 1 to 3,600 s, 300 s by default, keyed on the identity sources ([Lambda authorizers](https://docs.aws.amazon.com/apigateway/latest/developerguide/apigateway-use-lambda-authorizer.html)). On an HTTP API the authorizer may return a simple `isAuthorized` answer with a context; with caching on, add `$context.routeKey` to the identity sources or one route's allow serves every route ([HTTP API Lambda authorizers](https://docs.aws.amazon.com/apigateway/latest/developerguide/http-api-lambda-authorizer.html)). A Lambda authorizer is the place for a check the JWT authorizer cannot express and that must happen before the function, such as an API key looked up in a store; it is a second function with its own cold start, so prefer the JWT authorizer when a token suffices.

**Throttling.** The account limit is 10,000 requests per second with a burst of 5,000 per Region, shared by every API in it. Set lower limits per stage and per route or method for what an operation can bear, and on a REST API per client through usage plans; the tightest applicable limit wins ([request throttling](https://docs.aws.amazon.com/apigateway/latest/developerguide/api-gateway-request-throttling.html)). A throttled request is a `429` from the gateway.

**Invoke permission.** The gateway invokes through the function's resource-based policy. Grant `lambda:InvokeFunction` to `apigateway.amazonaws.com` with `AWS:SourceArn` naming the API, stage, method and path, not the account, so that only that route may call the function ([API Gateway permissions](https://docs.aws.amazon.com/lambda/latest/dg/services-apigateway.html)). This matters more than it looks: `Access` trusts the claims in the request context, and any principal allowed to invoke the function directly could write that context. Keep invoke permissions as narrow as the authorizer.

**How `davidrs` reads what the gateway verified.** The authorizer writes the claims into the request context, `requestContext.authorizer.jwt.claims` on an HTTP API, `authorizer.lambda` for a Lambda authorizer's context and `authorizer.claims` on a REST API, and [`Access`](crate::http::access::Access) reads them by default. The function does no token work: no key set, no signature, no clock.

```rust
use std::sync::Arc;

use davidrs::http::access::{Access, Claims, Grant};
use davidrs::http::{Api, Failure, Json, PlainErrors, Request};
use davidrs::Context;

/// The caller, built from claims the authorizer already verified.
struct User {
    id: String,
    groups: Vec<String>,
}

fn user(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.subject()?.to_owned(),
        groups: claims.list("cognito:groups"),
    })
}

/// The route's scope was checked by the authorizer's `authorizationScopes`;
/// the gateway does not evaluate groups, so the permission rule reads them.
fn billing() -> Access<User, User, ()> {
    Access::new(user)
        .require_caller()
        .permit(|user, _tenant| user.is_some_and(|user| user.groups.iter().any(|group| group == "billing")))
}

async fn invoices(_app: Arc<()>, _input: (), context: Context<Grant<User, ()>>) -> Result<Json<String>, Failure> {
    Ok(Json(format!("invoices of {}", context.scope().caller().id)))
}

let api = Api::new("list-invoices", billing(), PlainErrors);
# let _ = (api, |_request: &Request| Ok::<(), Failure>(()), invoices);
```

An HTTP API's JWT authorizer turns every claim into a string and writes arrays as `"[a b]"`; [`Claims::list`](crate::http::access::Claims::list) reads that form, a JSON array and a space-separated OAuth `scope` alike:

```rust
use davidrs::http::access::Claims;

let claims = Claims::new(serde_json::from_value(serde_json::json!({
    "sub": "user-1",
    "scope": "orders/read orders/write",
    "cognito:groups": "[billing admins]"
})).unwrap());
assert!(claims.contains("scope", "orders/write"));
assert_eq!(claims.list("cognito:groups"), ["billing", "admins"]);
```

With the AWS CDK, the recommended HTTP API is a few lines: the authorizer names the user pool as issuer and the app client as audience, and each route names the scope it needs.

```typescript
import { HttpApi, HttpMethod } from "aws-cdk-lib/aws-apigatewayv2";
import { HttpJwtAuthorizer } from "aws-cdk-lib/aws-apigatewayv2-authorizers";
import { HttpLambdaIntegration } from "aws-cdk-lib/aws-apigatewayv2-integrations";
import { RustFunction } from "cargo-lambda-cdk";

const listInvoices = new RustFunction(this, "ListInvoices", {
  manifestPath: "functions/list-invoices/Cargo.toml",
  binaryName: "list-invoices",
});

const users = new HttpJwtAuthorizer(
  "Users",
  `https://cognito-idp.${this.region}.amazonaws.com/${userPool.userPoolId}`,
  { jwtAudience: [appClient.userPoolClientId] }
);

const api = new HttpApi(this, "Api", { defaultAuthorizer: users });
api.addRoutes({
  path: "/invoices",
  methods: [HttpMethod.GET],
  integration: new HttpLambdaIntegration("ListInvoices", listInvoices),
  authorizationScopes: ["billing/read"],
});
```

## AWS WAF for APIs

A web ACL is an ordered list of rules evaluated on every request the protected resource receives; each rule counts, blocks, allows, challenges or serves a CAPTCHA. For an API, two kinds of rule do most of the work.

**Rate-based rules.** A rule counts requests per aggregation instance over an evaluation window of 60, 120, 300 or 600 seconds (300 by default) and applies its action once an instance exceeds the limit, which is at least 10. Aggregation is by source IP by default; the alternatives are the first address in a header such as `X-Forwarded-For`, the ASN, all requests matching a scope-down statement, or custom keys combined from a header, a cookie, a query argument, the query string, the URI path, the HTTP method, a JA3 or JA4 fingerprint, a label namespace, the IP or the forwarded IP ([aggregation options](https://docs.aws.amazon.com/waf/latest/developerguide/waf-rule-statement-type-rate-based-aggregation-options.html), [high-level settings](https://docs.aws.amazon.com/waf/latest/developerguide/waf-rule-statement-type-rate-based-high-level-settings.html)). No key reads inside a JWT: the `Authorization` header as a key limits per token, which rotates hourly and says nothing about the user or tenant behind it. The rule is also approximate by design: it applies the limit near the configured value, may take up to several minutes to start and usually under 30 seconds, and resets its counts when its settings change ([caveats](https://docs.aws.amazon.com/waf/latest/developerguide/waf-rule-statement-type-rate-based-caveats.html)). That is the right tool against volume and the wrong one for a quota: a plan of 1,000 exports a day per tenant, counted exactly and reported in `RateLimit` headers, is what [`RateLimited`](crate::http::RateLimited) is for.

```yaml
Rules:
  - Name: per-address
    Priority: 0
    Action:
      Block: {}
    Statement:
      RateBasedStatement:
        AggregateKeyType: IP
        Limit: 300
        EvaluationWindowSec: 60
    VisibilityConfig:
      SampledRequestsEnabled: true
      CloudWatchMetricsEnabled: true
      MetricName: per-address
  - Name: common
    Priority: 1
    OverrideAction:
      None: {}
    Statement:
      ManagedRuleGroupStatement:
        VendorName: AWS
        Name: AWSManagedRulesCommonRuleSet
    VisibilityConfig:
      SampledRequestsEnabled: true
      CloudWatchMetricsEnabled: true
      MetricName: common
```

**Managed rule groups worth enabling for an API** ([AWS Managed Rules list](https://docs.aws.amazon.com/waf/latest/developerguide/aws-managed-rule-groups-list.html)):

- **Core rule set** (`AWSManagedRulesCommonRuleSet`): general web exploits and oversized components. Start it in count mode, because an API that accepts JSON bodies with URLs or markup inside can trigger rules meant for HTML forms.
- **Known bad inputs** (`AWSManagedRulesKnownBadInputsRuleSet`): request patterns known to be invalid or to probe for specific vulnerabilities.
- **Amazon IP reputation list** and **Anonymous IP list**: sources associated with bots and abuse, and VPNs, proxies, Tor and hosting providers. The second one also refuses some legitimate callers, so count it first and decide per API.
- **Bot Control**, in its common level, classifies self-identifying bots; the targeted level adds fingerprinting, challenges and its own rate limiting. It costs extra and inspects every request unless a scope-down statement narrows it.
- **Account takeover prevention** and **account creation fraud prevention** belong on the sign-in and sign-up paths, where they read the request body; they are not for an API in general, and a user pool cannot use the takeover group at all.
- **Anti-DDoS** (`AWSManagedRulesAntiDDoSRuleSet`): layer 7 flood detection with silent browser challenges and blocking, placed near the top of the list without a scope-down statement so it sees the whole baseline. API clients that are not browsers cannot answer a challenge; exempt their paths with the URI expression, or turn the challenge off and keep only the block action.

**Body inspection.** WAF inspects the first 16 KB of a request body on CloudFront, API Gateway, Cognito, App Runner and Verified Access resources, raisable to 32, 48 or 64 KB for a fee, and 8 KB, fixed, on an Application Load Balancer or AppSync; headers and cookies are inspected up to 8 KB or 200 entries. The rest still reaches the origin, and each rule that inspects the body says what to do with an oversized one ([body inspection](https://docs.aws.amazon.com/waf/latest/developerguide/web-acl-setting-body-inspection-limit.html), [oversize components](https://docs.aws.amazon.com/waf/latest/developerguide/waf-oversize-request-components.html)). A WAF limit is therefore not a body limit for the function: set [`Api::body_limit`](crate::http::Api::body_limit) to what the route needs.

**Cost.** At the time of writing, a web ACL costs $5 per month, each rule or managed rule group $1 per month, and requests $0.60 per million, with extra fees for Bot Control, the fraud control groups, Anti-DDoS, capacity beyond 1,500 WCUs and body inspection beyond 16 KB ([AWS WAF pricing](https://aws.amazon.com/waf/pricing/)). For an API that handles a few million requests a month, the baseline is tens of dollars; the managed groups with per-request analysis are what to size deliberately.

## Amazon Cognito

A user pool is an OpenID Connect issuer: `https://cognito-idp.<region>.amazonaws.com/<pool-id>`, with its key set at `/.well-known/jwks.json` under it. That issuer URL and an app client id are the whole configuration of a JWT authorizer, and the same two values, with the audience and `token_use` you accept, configure a [`Verifier`](crate::auth::Verifier).

**Access token, not ID token.** A user pool issues three tokens per sign-in ([user pool tokens](https://docs.aws.amazon.com/cognito/latest/developerguide/amazon-cognito-user-pools-using-tokens-with-identity-providers.html)). The ID token says who signed in, carries attributes such as `email`, and has `aud` set to the app client id: it is issued to the client. The access token is what OAuth issues for calling a resource: `token_use` is `access`, it carries `scope`, `client_id` and `cognito:groups`, and it has no `aud` unless the client asked for resource binding, in which case `aud` is the resource URL it named ([resource binding](https://docs.aws.amazon.com/cognito/latest/developerguide/cognito-user-pools-define-resource-servers.html)). Accept the access token: require scopes on the route so the gateway refuses ID tokens, and in a verifier require `token_use` = `access` and, with resource binding, your own URL as audience.

**Groups and scopes for route authorization.** Custom scopes are declared on a resource server and written `identifier/scope`; an app client may request them, and the JWT authorizer's `authorizationScopes` or a REST method's OAuth scopes require them. Groups appear in both tokens as `cognito:groups` and are ignored by the gateway; read them in [`Access::permit`](crate::http::access::Access::permit). A pre token generation trigger adds claims of your own to either token.

**Machine to machine.** A confidential app client with a secret obtains an access token with the client credentials grant at the pool's token endpoint, or with the `GetClientToken` API; the token carries only custom scopes, names no user and comes with no refresh or ID token. M2M usage is billed per active client and per token request, so cache tokens for their lifetime ([scopes, M2M and resource servers](https://docs.aws.amazon.com/cognito/latest/developerguide/cognito-user-pools-define-resource-servers.html)). Resource binding is not available on this grant, so the verifier for a service caller checks `client_id` and the scope instead of `aud`.

**Threat protection.** On the Plus plan, compromised credentials detection compares passwords with leaked sets at sign-up, sign-in and password change, and adaptive authentication scores each sign-in by device and location to block or require a second factor. Both cover username and password authentication, not the secure remote password or custom flows, and none of it applies to federated users ([threat protection](https://docs.aws.amazon.com/cognito/latest/developerguide/cognito-user-pool-settings-threat-protection.html)).

**Token lifetimes and revocation.** Access and ID tokens live 5 minutes to 1 day, 60 minutes by default; refresh tokens 60 minutes to 10 years, 30 days by default ([refresh tokens](https://docs.aws.amazon.com/cognito/latest/developerguide/amazon-cognito-user-pools-using-the-refresh-token.html)). Revoking a refresh token, through `RevokeToken`, the `/oauth2/revoke` endpoint or a global sign-out, invalidates the tokens it produced for Cognito's own APIs, but a revoked JWT still verifies by signature and expiry, which is exactly what an API Gateway authorizer and a [`Verifier`](crate::auth::Verifier) do ([token revocation](https://docs.aws.amazon.com/cognito/latest/developerguide/token-revocation.html)). Keep access tokens short, and check revocation server-side only where a session must end at once.

**WAF on the user pool.** A web ACL associates with a user pool and inspects requests to managed login, the classic hosted UI and the pool's API endpoints, so credential stuffing is stopped before it reaches the authentication flow; the body of managed login requests is not forwarded, and the account takeover prevention group cannot be used ([WAF with user pools](https://docs.aws.amazon.com/cognito/latest/developerguide/user-pool-waf.html)).

## Use cases, end to end

Each one names the AWS setup we recommend and exactly which `davidrs` features to use and which to skip.

**A public read API.** CloudFront in front of an HTTP API, a web ACL on the distribution with a rate-based rule per address, the core rule set and the IP reputation list, route throttling sized to the function, caching where responses allow it. In the function: [`Public`](crate::http::Public), a small [`body_limit`](crate::http::Api::body_limit) and typed query decoding; a [`Cors`](crate::http::stream::Cors) or the gateway's CORS configuration, not both. Skip [`RateLimited`](crate::http::RateLimited) unless one operation is expensive enough to deserve its own quota. Skip token verification entirely.

**A signed-in SaaS API with tenants.** An HTTP API with a JWT authorizer on the user pool, scopes on every route, throttling per stage; CloudFront and a web ACL in front when the API is exposed to the open internet. In the function: [`Access`](crate::http::access::Access) with gateway claims, [`Tenancy::header`](crate::http::access::Tenancy::header) with the membership rule, [`require_tenant`](crate::http::access::Access::require_tenant), and a [`permit`](crate::http::access::Access::permit) rule on groups. The invoke permission names the API. Skip `verify_bearer`: the gateway did it, and a second verification would only add a key fetch. Use [`RateLimited`](crate::http::RateLimited) for plan quotas, keyed on what the authorizer verified rather than on a header a caller could choose:

```rust,no_run
# fn build(client: aws_sdk_dynamodb::Client) {
use std::time::Duration;

use davidrs::http::access::Claims;
use davidrs::http::rate_limit::DynamoWindow;
use davidrs::http::{Api, PlainErrors, Public, RateLimitConfig, RateLimited, Request};

/// The subject the authorizer verified, or one shared bucket for anything
/// else. Admission runs before the policy, so it reads the claims the
/// gateway wrote into the request context, never a header the caller wrote.
fn verified_subject(request: &Request<'_>) -> String {
    Claims::from_gateway(request)
        .and_then(|claims| claims.subject().map(str::to_owned))
        .unwrap_or_else(|| "anonymous".to_owned())
}

/// 1,000 exports per user per day: a quota no firewall can count.
let quota = RateLimitConfig::new("exports", 1_000, Duration::from_secs(86_400));
let api = Api::new("start-export", Public, PlainErrors)
    .admission(RateLimited::new(DynamoWindow::new(client, "quotas"), quota).key(verified_subject));
# let _ = api;
# }
```

**A partner API with API keys and usage plans.** A REST API: an API key per partner in a usage plan with a rate, a burst and a monthly quota, request validators with a model per operation, a web ACL on the stage, access logs to Firehose, mutual TLS on the custom domain when partners can hold a certificate. In the function: `apigw-rest`, and an [`Access`](crate::http::access::Access) policy whose caller is the partner named by the authorizer or by the key's context. Skip [`RateLimited`](crate::http::RateLimited) for volume: the plan counts it; keep it only for a quota the plan cannot express, such as a per-operation limit.

**Webhooks from a third party.** A Function URL with auth type `NONE`, because the sender cannot sign requests or hash bodies, with reserved concurrency as the cap and CloudFront plus a web ACL in front when the sender publishes its address ranges. In the function: a small [`body_limit`](crate::http::Api::body_limit), typed decoding, and a [`Policy`](crate::http::Policy) that verifies the sender's signature over the raw body before the handler runs. The body limit is checked before the policy, so the signature is computed over at most that many bytes.

```rust
use std::sync::Arc;

use base64::prelude::*;
use davidrs::http::{Api, Failure, NoContent, PlainErrors, Policy, Request, StatusCode};
use davidrs::{Context, Invocation};
use ring::hmac;

/// Evidence that the signature matched: only the policy builds one.
struct Signed;

/// Verifies `X-Signature`, an HMAC-SHA256 of the raw body in base64, with the
/// shared secret the sender was given. Every failure is the same `401`.
struct SenderSignature {
    key: hmac::Key,
}

impl Policy for SenderSignature {
    type Scope = Signed;

    async fn authorize(&self, request: &Request<'_>, _: &Invocation) -> Result<Signed, Failure> {
        let refused = || Failure::new(StatusCode::UNAUTHORIZED, "ERROR_BAD_SIGNATURE", "Signature mismatch");
        let signature = request.header("x-signature").ok_or_else(refused)?;
        let signature = BASE64_STANDARD.decode(signature).map_err(|_| refused())?;
        hmac::verify(&self.key, request.raw_body(), &signature).map_err(|_| refused())?;
        Ok(Signed)
    }
}

#[derive(serde::Deserialize)]
struct Delivery {
    id: String,
}

async fn record(_app: Arc<()>, delivery: Delivery, _context: Context<Signed>) -> Result<NoContent, Failure> {
    let _ = delivery.id;
    Ok(NoContent)
}

let key = hmac::Key::new(hmac::HMAC_SHA256, b"the shared secret, read from Secrets Manager");
let api = Api::new("webhook", SenderSignature { key }, PlainErrors).body_limit(64 * 1024);
# let _ = (api, |request: &Request| request.json::<Delivery>(), record);
```

`ring` is already linked by the `auth` feature; add it to your own manifest to name it. Read the secret with [`secrets`](crate::secrets) in `main`, make the handler idempotent on the delivery id, and answer quickly: senders retry on any error and on a timeout.

**A streamed endpoint.** A Function URL in `RESPONSE_STREAM` mode behind CloudFront with origin access control, a web ACL on the distribution, an origin response timeout equal to the longest stream, reserved concurrency as the cap. In the function: [`StreamApi`](crate::http::stream::StreamApi) with [`Cors`](crate::http::stream::Cors) listing `x-amz-content-sha256` when browsers `POST`, [`StreamApi::prepare`](crate::http::stream::StreamApi::prepare) restoring the bearer header the edge function relocated, and [`Access::verify_bearer`](crate::http::access::Access::verify_bearer) with gateway claims off. A streamed route has no admission stage, so the handler calls its [`Counter`](crate::http::Counter) itself once it knows who is calling. A REST API in `STREAM` mode is the alternative when you need the gateway's authorizer more than the URL's simplicity.

**An MCP server for AI clients.** A Function URL with auth type `NONE` behind CloudFront and a web ACL, caching disabled, all viewer headers except `Host` forwarded; OAC does not fit because MCP clients cannot hash bodies. In the function: [`mcp::Server`](crate::mcp::Server) with an [`Access`](crate::http::access::Access) policy that verifies every bearer token against the pool with your server's URL as audience, a [`ProtectedResource`](crate::mcp::ProtectedResource) so a `401` tells the client where to sign in, and a [`RateLimited`](crate::http::RateLimited) admission. The [MCP chapter](crate::guide::mcp) covers the OAuth flow, and the HTTP API alternative with a JWT authorizer for clients that fit in 30 seconds.

**Internal service-to-service calls.** A Function URL with auth type `AWS_IAM`, or an API Gateway route with IAM authorization; the caller's role holds `lambda:InvokeFunctionUrl` and `lambda:InvokeFunction`, or `execute-api:Invoke`, and signs with SigV4. In the function: a [`Policy`](crate::http::Policy) that reads the signer from `requestContext.authorizer.iam` if the operation cares who called, otherwise [`Public`](crate::http::Public). Skip tokens, key sets and `RateLimited`; IAM decided, and the only limit that matters is reserved concurrency.

**SQS and EventBridge consumers.** `ReportBatchItemFailures` on the event source mapping, a visibility timeout of at least six times the function timeout, a redrive policy with `maxReceiveCount` of at least 5 and a dead-letter queue ([SQS event source](https://docs.aws.amazon.com/lambda/latest/dg/services-sqs-configure.html)); an on-failure destination for EventBridge targets. In the function: [`queue::run`](crate::queue::run) and [`event::run`](crate::event::run), a typed payload, an idempotent handler. There is no caller to authenticate: the producer's right to send was IAM's decision on the queue or the bus, and the function's role decides what the consumer may do. See [queues](crate::guide::queues) and [triggers](crate::guide::triggers).

**A function invoked directly.** `lambda:InvokeFunction` granted to exactly the principals that may call, nothing else. In the function: [`runtime::run`](crate::runtime::run) with a typed payload. Nothing verifies a token, and an [`Access`](crate::http::access::Access) policy behind an HTTP pipeline would trust whatever request context the invoker wrote, which is why the invoke permission is the control here.

## What `davidrs` does in every setup

These stay on behind a perfect gateway, because each guards against something the gateway cannot see.

- **Bounded decoding and body limits per route.** [`Api::body_limit`](crate::http::Api::body_limit) is checked before any decoder runs, including one that reads raw bytes; tokens, key sets, upstream answers, decompressed payloads and MCP bodies have their own bounds. The gateway's 10 MB and the firewall's 16 KB are platform numbers; the route's number is the one that reflects its purpose.
- **`5xx` redaction.** A gateway cannot redact a body the function wrote. [`Failure::public_message`](crate::http::Failure::public_message) is the only accessor a renderer has, and it returns [`INTERNAL_MESSAGE`](crate::http::INTERNAL_MESSAGE) for every `5xx`, however the failure was built:

```rust
use davidrs::http::{Failure, StatusCode, INTERNAL_MESSAGE};

let failure = Failure::internal("FAULT_STORE", "ProvisionedThroughputExceeded on table orders");
assert_eq!(failure.status(), StatusCode::INTERNAL_SERVER_ERROR);
assert_eq!(failure.public_message(), INTERNAL_MESSAGE);
```

- **Absolute deadlines.** One [`Deadline`](crate::Deadline) per invocation, with a reserve before Lambda's own timeout, so the client receives a rendered `504` instead of a cut connection, a retry never gets a fresh allowance, and a streamed producer stops even when the client keeps reading. The gateway's timeout is the outer bound; the function's deadline is the one that answers.
- **Tenant membership.** The authorizer proves who is calling; [`Tenancy`](crate::http::access::Tenancy) proves they may act for the tenant they named, before the handler runs, and the [`Grant`](crate::http::access::Grant) type makes it impossible to forget.
- **Business quotas.** [`RateLimited`](crate::http::RateLimited) counts exactly, under a key of the application's choosing, and tells the client its remaining budget; it fails closed when its counter is unreachable. The firewall's rate rule protects availability; this protects the plan.
- **Least-privilege clients.** One function, one operation, one role, with exactly the actions and resources that operation uses; the SDK clients built in `main` can do nothing the role does not allow, and every feature links only what it names, so a read endpoint carries no write client.
- **No secrets in errors.** [`secrets`](crate::secrets) errors name the secret and never its value, environment errors name the variable, outbound client errors carry no URL, and the pipelines log a failure's code and kind, never its message or detail. Whatever a log pipeline or a support engineer sees, it is not a credential.
