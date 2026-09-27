//! The configurable access policy: identifying callers from gateway claims,
//! requiring them, selecting tenants and applying permission rules.
#![cfg(feature = "http")]

mod support;

use std::sync::Arc;

use davidrs::http::access::{Access, Claims, Grant, Refusals, Requirement, Tenancy};
use davidrs::http::{
    codes, Api, ErrorDefinition, Failure, FailureKind, Json, PlainErrors, Policy, Request,
    StatusCode,
};
use davidrs::{Context, Deadline, Invocation};
use serde_json::{json, Value};

/// The application's caller in these tests.
#[derive(Debug, Clone, PartialEq)]
struct User {
    id: String,
    tenants: Vec<String>,
    admin: bool,
}

/// Claims describe a user when they carry a subject.
fn user(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.subject()?.to_owned(),
        tenants: claims.list("tenants"),
        admin: claims.contains("groups", "admin"),
    })
}

/// Membership in a tenant, as the application defines it.
fn member(user: &User, tenant: &str) -> bool {
    user.tenants.iter().any(|own| own == tenant)
}

/// An HTTP API event, optionally carrying an authorizer and headers.
fn http_api(authorizer: Option<Value>, headers: Value) -> lambda_http::Request {
    let mut context = json!({
        "http": { "method": "GET", "path": "/orders", "sourceIp": "203.0.113.9" },
        "requestId": "r1",
        "stage": "$default",
        "timeEpoch": 0
    });
    if let Some(authorizer) = authorizer {
        context["authorizer"] = authorizer;
    }
    let event = json!({
        "version": "2.0",
        "rawPath": "/orders",
        "rawQueryString": "",
        "headers": headers,
        "requestContext": context,
        "isBase64Encoded": false
    });
    lambda_http::request::from_str(&event.to_string()).expect("an HTTP API event")
}

/// An HTTP API request whose JWT authorizer verified these claims.
fn signed_in(claims: Value, headers: Value) -> lambda_http::Request {
    http_api(
        Some(json!({ "jwt": { "claims": claims, "scopes": null } })),
        headers,
    )
}

fn alice() -> Value {
    json!({ "sub": "alice", "tenants": "[tenant-a tenant-b]", "groups": "[admin]" })
}

fn invocation() -> Invocation {
    Invocation::new("r1", Deadline::after(std::time::Duration::from_secs(5)))
}

async fn authorize<P: Policy>(
    policy: &P,
    request: &lambda_http::Request,
) -> Result<P::Scope, Failure> {
    policy
        .authorize(&Request::new(request), &invocation())
        .await
}

fn refused<T>(result: Result<T, Failure>) -> Failure {
    match result {
        Ok(_) => panic!("the policy should have refused"),
        Err(failure) => failure,
    }
}

#[tokio::test]
async fn an_optional_caller_is_identified_from_http_api_jwt_claims() {
    let grant = authorize(&Access::new(user), &signed_in(alice(), json!({})))
        .await
        .expect("grant");
    let caller = grant.caller().as_ref().expect("a caller");
    assert_eq!(caller.id, "alice");
    assert_eq!(caller.tenants, ["tenant-a", "tenant-b"]);
    assert!(caller.admin);
    assert_eq!(grant.tenant(), &());
}

#[tokio::test]
async fn an_anonymous_request_passes_an_optional_policy_without_a_caller() {
    let grant = authorize(&Access::new(user), &http_api(None, json!({})))
        .await
        .expect("grant");
    assert!(grant.caller().is_none());
}

#[tokio::test]
async fn a_request_built_by_hand_without_a_request_context_is_anonymous() {
    let request = lambda_http::http::Request::builder()
        .uri("https://example.com/orders")
        .body(lambda_http::Body::Empty)
        .expect("request");
    let grant = authorize(&Access::new(user), &request)
        .await
        .expect("grant");
    assert!(grant.caller().is_none());
}

#[tokio::test]
async fn a_required_caller_is_refused_with_401_when_nobody_is_identified() {
    let failure = refused(
        authorize(
            &Access::new(user).require_caller(),
            &http_api(None, json!({})),
        )
        .await,
    );
    assert_eq!(failure.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(failure.code(), codes::UNAUTHENTICATED);
    assert_eq!(failure.kind(), FailureKind::Policy);
}

#[tokio::test]
async fn a_required_caller_reaches_the_grant_unwrapped() {
    let grant: Grant<User, ()> = authorize(
        &Access::new(user).require_caller(),
        &signed_in(alice(), json!({})),
    )
    .await
    .expect("grant");
    assert_eq!(grant.caller().id, "alice");
    let (caller, tenant) = grant.into_parts();
    assert_eq!((caller.id.as_str(), tenant), ("alice", ()));
}

#[tokio::test]
async fn claims_that_describe_no_caller_leave_the_request_anonymous() {
    let request = signed_in(json!({ "email": "no-subject@example.com" }), json!({}));
    assert!(authorize(&Access::new(user), &request)
        .await
        .expect("grant")
        .caller()
        .is_none());
    let failure = refused(authorize(&Access::new(user).require_caller(), &request).await);
    assert_eq!(failure.code(), codes::UNAUTHENTICATED);
}

#[tokio::test]
async fn gateway_claims_can_be_ignored() {
    let policy = Access::new(user).gateway_claims(false);
    let grant = authorize(&policy, &signed_in(alice(), json!({})))
        .await
        .expect("grant");
    assert!(
        grant.caller().is_none(),
        "claims are not trusted when turned off"
    );
}

#[tokio::test]
async fn a_lambda_authorizer_context_is_read_as_claims() {
    let request = http_api(
        Some(json!({ "lambda": { "sub": "bob", "tenants": ["tenant-a"] } })),
        json!({}),
    );
    let grant = authorize(&Access::new(user).require_caller(), &request)
        .await
        .expect("grant");
    assert_eq!(grant.caller().id, "bob");
    assert_eq!(grant.caller().tenants, ["tenant-a"]);
}

#[tokio::test]
async fn a_member_selects_a_tenant_with_the_header() {
    let policy = Access::new(user)
        .require_caller()
        .tenancy(Tenancy::header("x-tenant-id", member));
    let grant = authorize(
        &policy,
        &signed_in(alice(), json!({ "x-tenant-id": " tenant-b " })),
    )
    .await
    .expect("grant");
    assert_eq!(
        grant.tenant().as_deref(),
        Some("tenant-b"),
        "the header is trimmed"
    );
}

#[tokio::test]
async fn a_caller_outside_the_requested_tenant_is_refused_with_403() {
    let policy = Access::new(user).tenancy(Tenancy::header("x-tenant-id", member));
    let failure = refused(
        authorize(
            &policy,
            &signed_in(alice(), json!({ "x-tenant-id": "tenant-c" })),
        )
        .await,
    );
    assert_eq!(failure.status(), StatusCode::FORBIDDEN);
    assert_eq!(failure.code(), codes::FORBIDDEN);
    assert_eq!(failure.kind(), FailureKind::Policy);
}

#[tokio::test]
async fn an_anonymous_request_that_names_a_tenant_is_refused() {
    let policy = Access::new(user).tenancy(Tenancy::header("x-tenant-id", member));
    let failure = refused(
        authorize(
            &policy,
            &http_api(None, json!({ "x-tenant-id": "tenant-a" })),
        )
        .await,
    );
    assert_eq!(failure.status(), StatusCode::FORBIDDEN);
    assert_eq!(failure.code(), codes::FORBIDDEN);
}

/// Only a public tenancy lets an anonymous request name a tenant, as context.
#[tokio::test]
async fn a_public_tenancy_lets_an_anonymous_caller_name_a_tenant_as_context() {
    let policy = Access::new(user).tenancy(Tenancy::header("x-tenant-id", member).public());
    let grant = authorize(
        &policy,
        &http_api(None, json!({ "x-tenant-id": "tenant-a" })),
    )
    .await
    .expect("grant");
    assert!(grant.caller().is_none());
    assert_eq!(grant.tenant().as_deref(), Some("tenant-a"));
    let outsider = refused(
        authorize(
            &policy,
            &signed_in(alice(), json!({ "x-tenant-id": "tenant-c" })),
        )
        .await,
    );
    assert_eq!(
        outsider.code(),
        codes::FORBIDDEN,
        "members are still checked"
    );
}

#[tokio::test]
async fn without_a_header_the_fallback_names_the_callers_tenant() {
    let only = |user: &User| (user.tenants.len() == 1).then(|| user.tenants[0].clone());
    let policy = Access::new(user).tenancy(Tenancy::header("x-tenant-id", member).or_else(only));
    let single = signed_in(
        json!({ "sub": "carol", "tenants": "[tenant-a]" }),
        json!({}),
    );
    assert_eq!(
        authorize(&policy, &single)
            .await
            .expect("grant")
            .tenant()
            .as_deref(),
        Some("tenant-a")
    );
    let several = signed_in(alice(), json!({ "x-tenant-id": "   " }));
    assert_eq!(
        authorize(&policy, &several).await.expect("grant").tenant(),
        &None,
        "a blank header is absent, and two tenants give no default"
    );
}

#[tokio::test]
async fn without_a_header_or_a_fallback_no_tenant_is_selected() {
    let policy = Access::new(user).tenancy(Tenancy::header("x-tenant-id", member));
    assert_eq!(
        authorize(&policy, &signed_in(alice(), json!({})))
            .await
            .expect("grant")
            .tenant(),
        &None
    );
    assert_eq!(
        authorize(&policy, &http_api(None, json!({})))
            .await
            .expect("grant")
            .tenant(),
        &None
    );
}

#[tokio::test]
async fn a_required_tenant_is_refused_with_400_when_none_is_selected() {
    let policy = Access::new(user)
        .tenancy(Tenancy::header("x-tenant-id", member))
        .require_tenant();
    let failure = refused(authorize(&policy, &signed_in(alice(), json!({}))).await);
    assert_eq!(failure.status(), StatusCode::BAD_REQUEST);
    assert_eq!(failure.code(), codes::TENANT_REQUIRED);
    let grant: Grant<Option<User>, String> = authorize(
        &policy,
        &signed_in(alice(), json!({ "x-tenant-id": "tenant-a" })),
    )
    .await
    .expect("grant");
    assert_eq!(grant.tenant(), "tenant-a");
}

#[tokio::test]
async fn a_permission_rule_sees_the_caller_and_tenant_and_refuses_with_403() {
    let policy = Access::new(user)
        .tenancy(Tenancy::header("x-tenant-id", member).public())
        .permit(|user, tenant| user.is_some_and(|user| user.admin) && tenant == Some("tenant-a"));
    assert!(authorize(
        &policy,
        &signed_in(alice(), json!({ "x-tenant-id": "tenant-a" }))
    )
    .await
    .is_ok());
    let wrong_tenant = refused(
        authorize(
            &policy,
            &signed_in(alice(), json!({ "x-tenant-id": "tenant-b" })),
        )
        .await,
    );
    assert_eq!(wrong_tenant.code(), codes::FORBIDDEN);
    let anonymous = refused(
        authorize(
            &policy,
            &http_api(None, json!({ "x-tenant-id": "tenant-a" })),
        )
        .await,
    );
    assert_eq!(
        anonymous.status(),
        StatusCode::FORBIDDEN,
        "the rule decides for anonymous callers too"
    );
}

/// Each refusal happens at its own step, so a request with several problems
/// always gets the earliest one.
#[tokio::test]
async fn the_order_of_checks_decides_which_refusal_wins() {
    let strict = Access::new(user)
        .require_caller()
        .tenancy(Tenancy::header("x-tenant-id", member))
        .require_tenant()
        .permit(|_, _| false);
    let anonymous = refused(
        authorize(
            &strict,
            &http_api(None, json!({ "x-tenant-id": "tenant-c" })),
        )
        .await,
    );
    assert_eq!(
        anonymous.code(),
        codes::UNAUTHENTICATED,
        "the caller before the tenant"
    );
    let outsider = refused(
        authorize(
            &strict,
            &signed_in(alice(), json!({ "x-tenant-id": "tenant-c" })),
        )
        .await,
    );
    assert_eq!(
        outsider.code(),
        codes::FORBIDDEN,
        "membership before anything later"
    );
    let unselected = refused(authorize(&strict, &signed_in(alice(), json!({}))).await);
    assert_eq!(
        unselected.code(),
        codes::TENANT_REQUIRED,
        "the tenant before the permission rule"
    );
    let denied = refused(
        authorize(
            &strict,
            &signed_in(alice(), json!({ "x-tenant-id": "tenant-a" })),
        )
        .await,
    );
    assert_eq!(denied.code(), codes::FORBIDDEN, "the permission rule last");
}

#[tokio::test]
async fn an_application_keeps_its_own_refusal_codes() {
    let errors = Refusals {
        unauthenticated: ErrorDefinition::new(
            "ERROR_SIGN_IN",
            StatusCode::UNAUTHORIZED,
            "Please sign in",
        ),
        ..Refusals::default()
    };
    let failure = refused(
        authorize(
            &Access::new(user).require_caller().refusals(errors),
            &http_api(None, json!({})),
        )
        .await,
    );
    assert_eq!(
        (failure.code(), failure.public_message()),
        ("ERROR_SIGN_IN", "Please sign in")
    );
}

#[test]
fn the_default_refusals_use_the_crate_codes() {
    let errors = Refusals::default();
    assert_eq!(
        [
            errors.invalid_token,
            errors.unauthenticated,
            errors.forbidden,
            errors.tenant_required
        ]
        .map(|definition| (definition.code, definition.status.as_u16())),
        [
            (codes::INVALID_TOKEN, 401),
            (codes::UNAUTHENTICATED, 401),
            (codes::FORBIDDEN, 403),
            (codes::TENANT_REQUIRED, 400)
        ]
    );
}

#[tokio::test]
async fn the_grant_reaches_the_handler_through_the_pipeline_and_refusals_are_rendered() {
    async fn whoami(
        _app: Arc<()>,
        _input: (),
        context: Context<Grant<User, ()>>,
    ) -> Result<Json<Value>, Failure> {
        Ok(Json(json!({ "id": context.scope().caller().id })))
    }
    let api = Api::new("whoami", Access::new(user).require_caller(), PlainErrors);
    let decode = |_: &Request<'_>| Ok(());
    let ok = api
        .handle(
            Arc::new(()),
            signed_in(alice(), json!({})),
            &decode,
            &whoami,
        )
        .await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        ok.body(),
        &lambda_http::Body::Text(r#"{"id":"alice"}"#.to_owned())
    );
    let refused = api
        .handle(Arc::new(()), http_api(None, json!({})), &decode, &whoami)
        .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    let lambda_http::Body::Text(body) = refused.body() else {
        panic!("a text body")
    };
    assert!(body.contains(codes::UNAUTHENTICATED));
}

#[test]
fn a_requirement_states_whether_a_missing_value_is_refused() {
    assert_eq!(
        [
            <String as Requirement<String>>::REQUIRED,
            <Option<String> as Requirement<String>>::REQUIRED,
            <() as Requirement<String>>::REQUIRED,
        ],
        [true, false, false]
    );
    assert_eq!(<String as Requirement<String>>::fulfill(None), None);
    assert_eq!(
        <Option<String> as Requirement<String>>::fulfill(None),
        Some(None)
    );
    assert_eq!(
        <() as Requirement<String>>::fulfill(Some("tenant-a".to_owned())),
        Some(())
    );
}

#[test]
fn claims_read_every_shape_a_list_arrives_in() {
    let claims = Claims::new(
        serde_json::from_value(json!({
            "sub": "alice",
            "array": ["a", 1, true, null, {}],
            "json_text": "[\"a\",\"b\"]",
            "bracketed": "[a b]",
            "spaced": " a  b ",
            "commas": "a,b, c",
            "empty": "",
            "number": 7
        }))
        .expect("claims"),
    );
    assert_eq!(claims.list("array"), ["a", "1", "true"]);
    assert_eq!(claims.list("json_text"), ["a", "b"]);
    assert_eq!(claims.list("bracketed"), ["a", "b"]);
    assert_eq!(claims.list("spaced"), ["a", "b"]);
    assert_eq!(claims.list("commas"), ["a", "b", "c"]);
    assert!(claims.list("empty").is_empty());
    assert!(
        claims.list("number").is_empty(),
        "a scalar that is not text is not a list"
    );
    assert!(claims.list("missing").is_empty());
    assert!(claims.contains("commas", "c"));
    assert!(!claims.contains("commas", "d"));
}

#[test]
fn claims_expose_single_values_and_the_whole_set() {
    let map: serde_json::Map<String, Value> =
        serde_json::from_value(json!({ "sub": "alice", "level": 3 })).expect("claims");
    let claims = Claims::from(map.clone());
    assert_eq!(claims.subject(), Some("alice"));
    assert_eq!(claims.string("level"), None, "only strings are text");
    assert_eq!(claims.get("level"), Some(&json!(3)));
    assert_eq!(claims.all(), &map);
    assert_eq!(Claims::default().subject(), None);
}

#[test]
fn debug_output_shows_the_configuration_and_no_closures() {
    let policy = Access::new(user).tenancy(
        Tenancy::header("x-tenant-id", member)
            .or_else(|_| None)
            .public(),
    );
    let text = format!("{policy:?}");
    assert!(text.contains("gateway_claims: true"), "{text}");
    assert!(text.contains("gateway_token_claims: false"), "{text}");
    assert!(text.contains("x-tenant-id"), "{text}");
    assert!(text.contains("fallback: true"), "{text}");
    assert!(text.contains("public: true"), "{text}");
}

/// The issuer of the tokens an authorizer verified in these tests.
const GATEWAY_ISSUER: &str = "https://id.example.com";

/// A JWT as the authorizer received it. The signature is never read here.
fn jwt(claims: &Value) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.c2ln",
        b64.encode(r#"{"alg":"RS256","kid":"k1"}"#),
        b64.encode(claims.to_string())
    )
}

/// An `Authorization` header carrying `claims` as a bearer token.
fn bearer(claims: &Value) -> Value {
    json!({ "authorization": format!("Bearer {}", jwt(claims)) })
}

/// Claims whose realm roles are nested in an object, as Keycloak writes them.
fn nested() -> Value {
    json!({
        "iss": GATEWAY_ISSUER,
        "sub": "alice",
        "exp": 4_102_444_800_u64,
        "realm_access": { "roles": ["admin"] }
    })
}

/// What an HTTP API JWT authorizer passes on for [`nested`]: strings only.
fn flattened() -> Value {
    json!({
        "iss": GATEWAY_ISSUER,
        "sub": "alice",
        "exp": "4102444800",
        "realm_access": "map[roles:[admin]]"
    })
}

#[test]
fn a_gateway_verified_token_keeps_the_json_types_of_its_claims() {
    let request = signed_in(flattened(), bearer(&nested()));
    let claims = Claims::from_gateway_token(&Request::new(&request)).expect("claims");
    assert_eq!(
        claims.get("realm_access"),
        Some(&json!({ "roles": ["admin"] }))
    );
    assert_eq!(claims.get("exp"), Some(&json!(4_102_444_800_u64)));
}

/// A token is read only where an authorizer ran and only when it is the
/// token the authorizer verified: no authorizer, another subject, another
/// issuer, gateway claims without an issuer, a value that is not a JWT, an
/// oversized token and a missing header all read as `None`.
#[test]
fn a_bearer_token_is_read_only_when_it_matches_the_gateway_claims() {
    let mut other_subject = nested();
    other_subject["sub"] = json!("mallory");
    let mut other_issuer = nested();
    other_issuer["iss"] = json!("https://id.example.org");
    let mut oversized = nested();
    oversized["padding"] = json!("x".repeat(9 * 1024));
    for request in [
        http_api(None, bearer(&nested())),
        signed_in(flattened(), bearer(&other_subject)),
        signed_in(flattened(), bearer(&other_issuer)),
        signed_in(json!({ "sub": "alice" }), bearer(&nested())),
        signed_in(flattened(), json!({ "authorization": "Bearer not-a-jwt" })),
        signed_in(flattened(), bearer(&oversized)),
        signed_in(flattened(), json!({})),
    ] {
        assert_eq!(Claims::from_gateway_token(&Request::new(&request)), None);
    }
}

#[tokio::test]
async fn gateway_token_claims_give_the_policy_nested_claims() {
    let admin = |claims: &Claims| {
        let roles = claims
            .get("realm_access")?
            .get("roles")?
            .as_array()?
            .clone();
        Some(roles.contains(&json!("admin")))
    };
    let request = signed_in(flattened(), bearer(&nested()));
    let flat = authorize(&Access::new(admin), &request)
        .await
        .expect("grant");
    assert_eq!(flat.caller(), &None);
    let structured = authorize(&Access::new(admin).gateway_token_claims(true), &request)
        .await
        .expect("grant");
    assert_eq!(structured.caller(), &Some(true));
}

/// A request whose token cannot be read falls back to the authorizer's own
/// claims.
#[tokio::test]
async fn gateway_token_claims_fall_back_to_the_authorizer_claims() {
    let request = signed_in(alice(), json!({}));
    let grant = authorize(&Access::new(user).gateway_token_claims(true), &request)
        .await
        .expect("grant");
    assert_eq!(
        grant.caller().as_ref().map(|user| user.id.as_str()),
        Some("alice")
    );
}

/// A REST API event whose authorizer attached these fields.
#[cfg(feature = "apigw-rest")]
fn rest_api(authorizer: Value) -> lambda_http::Request {
    let event = json!({
        "resource": "/orders",
        "path": "/orders",
        "httpMethod": "GET",
        "headers": {},
        "multiValueHeaders": {},
        "queryStringParameters": null,
        "requestContext": {
            "resourcePath": "/orders",
            "httpMethod": "GET",
            "path": "/prod/orders",
            "stage": "prod",
            "requestId": "r1",
            "accountId": "123456789012",
            "identity": { "sourceIp": "203.0.113.9" },
            "authorizer": authorizer
        },
        "body": null,
        "isBase64Encoded": false
    });
    lambda_http::request::from_str(&event.to_string()).expect("a REST API event")
}

#[cfg(feature = "apigw-rest")]
#[tokio::test]
async fn a_rest_api_cognito_authorizer_provides_its_claims() {
    let request = rest_api(json!({ "claims": { "sub": "dave", "cognito:groups": "admin,staff" } }));
    let grant = authorize(&Access::new(user).require_caller(), &request)
        .await
        .expect("grant");
    assert_eq!(grant.caller().id, "dave");
}

#[cfg(feature = "apigw-rest")]
#[tokio::test]
async fn a_rest_api_lambda_authorizer_context_is_read_as_claims() {
    let request = rest_api(json!({ "principalId": "erin", "sub": "erin", "tenants": "tenant-a" }));
    let grant = authorize(&Access::new(user).require_caller(), &request)
        .await
        .expect("grant");
    assert_eq!(
        (
            grant.caller().id.as_str(),
            grant.caller().tenants.as_slice()
        ),
        ("erin", &["tenant-a".to_owned()][..])
    );
}

#[cfg(feature = "apigw-rest")]
#[tokio::test]
async fn a_rest_api_request_without_an_authorizer_is_anonymous() {
    assert!(authorize(&Access::new(user), &rest_api(Value::Null))
        .await
        .expect("grant")
        .caller()
        .is_none());
}

/// Bearer tokens verified by the function itself, against a local JWKS.
#[cfg(feature = "auth")]
mod bearer {
    use std::sync::Arc;

    use davidrs::http::access::Access;
    use davidrs::http::codes;
    use serde_json::json;

    use super::support::tokens::{rs256, token, Jwks, ISSUER};
    use super::{authorize, http_api, refused, signed_in, user};

    /// The audience every test token is issued for.
    const AUDIENCE: &str = "orders-api";

    /// A verifier over a JWKS publishing the test key as `k1`.
    async fn verifier() -> (Jwks, Arc<davidrs::auth::Verifier>) {
        let jwks = Jwks::serving(vec![super::support::tokens::jwk("k1")]).await;
        let verifier = Arc::new(
            jwks.load(
                davidrs::auth::VerifierConfig::new(ISSUER, jwks.server.url("/jwks"))
                    .with_audiences(vec![AUDIENCE.to_owned()]),
            )
            .await,
        );
        (jwks, verifier)
    }

    /// A signed token for `sub`, valid for an hour.
    fn signed_for(sub: &str) -> String {
        let claims = json!({ "iss": ISSUER, "aud": AUDIENCE, "sub": sub, "exp": super::support::tokens::now() + 3600 });
        token(&rs256("k1"), &claims)
    }

    #[tokio::test]
    async fn a_verified_bearer_token_identifies_the_caller_in_either_header_form() {
        let (_jwks, verifier) = verifier().await;
        let policy = Access::new(user).require_caller().verify_bearer(verifier);
        for header in [
            format!("Bearer {}", signed_for("frank")),
            format!("bearer  {}", signed_for("frank")),
            signed_for("frank"),
        ] {
            let grant = authorize(&policy, &http_api(None, json!({ "authorization": header })))
                .await
                .expect("grant");
            assert_eq!(grant.caller().id, "frank");
        }
    }

    #[tokio::test]
    async fn a_token_that_does_not_verify_is_refused_before_anything_else() {
        let (_jwks, verifier) = verifier().await;
        let policy = Access::new(user).require_caller().verify_bearer(verifier);
        let failure = refused(
            authorize(
                &policy,
                &http_api(None, json!({ "authorization": "Bearer not.a.token" })),
            )
            .await,
        );
        assert_eq!(failure.status(), davidrs::http::StatusCode::UNAUTHORIZED);
        assert_eq!(failure.code(), codes::INVALID_TOKEN);
    }

    #[tokio::test]
    async fn a_verified_token_whose_claims_describe_no_caller_is_invalid() {
        let (_jwks, verifier) = verifier().await;
        let claims =
            json!({ "iss": ISSUER, "aud": AUDIENCE, "exp": super::support::tokens::now() + 3600 });
        let request = http_api(
            None,
            json!({ "authorization": format!("Bearer {}", token(&rs256("k1"), &claims)) }),
        );
        let failure =
            refused(authorize(&Access::new(user).verify_bearer(verifier), &request).await);
        assert_eq!(failure.code(), codes::INVALID_TOKEN);
    }

    #[tokio::test]
    async fn another_authorization_scheme_is_not_a_caller_token() {
        let (jwks, verifier) = verifier().await;
        let before = jwks.fetches();
        let policy = Access::new(user).verify_bearer(verifier);
        for header in [
            "Basic dXNlcjpwYXNz",
            "Signed link-123",
            "Bearer ",
            "bearer",
            "   ",
        ] {
            let grant = authorize(&policy, &http_api(None, json!({ "authorization": header })))
                .await
                .expect("grant");
            assert!(
                grant.caller().is_none(),
                "{header:?} leaves the request anonymous"
            );
        }
        assert_eq!(jwks.fetches(), before, "nothing was verified");
    }

    #[tokio::test]
    async fn gateway_claims_win_over_a_bearer_token() {
        let (_jwks, verifier) = verifier().await;
        let request = signed_in(
            json!({ "sub": "alice" }),
            json!({ "authorization": format!("Bearer {}", signed_for("frank")) }),
        );
        let grant = authorize(&Access::new(user).verify_bearer(verifier), &request)
            .await
            .expect("grant");
        assert_eq!(
            grant.caller().as_ref().map(|user| user.id.as_str()),
            Some("alice")
        );
    }

    #[tokio::test]
    async fn without_gateway_trust_only_the_bearer_token_counts() {
        let (_jwks, verifier) = verifier().await;
        let request = signed_in(
            json!({ "sub": "alice" }),
            json!({ "authorization": format!("Bearer {}", signed_for("frank")) }),
        );
        let policy = Access::new(user)
            .gateway_claims(false)
            .verify_bearer(verifier);
        let grant = authorize(&policy, &request).await.expect("grant");
        assert_eq!(
            grant.caller().as_ref().map(|user| user.id.as_str()),
            Some("frank")
        );
        assert!(format!("{policy:?}").contains("verify_bearer: true"));
    }
}

#[test]
fn from_gateway_reads_the_claims_the_authorizer_verified_and_nothing_else() {
    let request = signed_in(alice(), json!({ "x-sub": "mallory" }));
    let claims = Claims::from_gateway(&Request::new(&request)).expect("claims");
    assert_eq!(claims.subject(), Some("alice"), "headers are never claims");
    assert_eq!(
        Claims::from_gateway(&Request::new(&http_api(None, json!({})))),
        None
    );
    let hand_built = lambda_http::http::Request::builder()
        .uri("https://example.com/orders")
        .body(lambda_http::Body::Empty)
        .expect("request");
    assert_eq!(Claims::from_gateway(&Request::new(&hand_built)), None);
}
