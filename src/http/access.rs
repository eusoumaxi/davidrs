//! Who may call, and for which tenant: a [`Policy`] built from configuration.
//!
//! Most APIs answer the same questions before a handler runs:
//!
//! 1. **Who is calling?** Claims an API Gateway authorizer already verified,
//!    or a bearer token the function verifies itself.
//! 2. **Must someone be calling?** Some routes are public, some serve
//!    anonymous and signed-in callers differently, some need a caller.
//! 3. **For which tenant?** A multi-tenant API lets a caller act for one of
//!    the tenants they belong to, usually selected with a header.
//! 4. **Is this caller allowed here?** A role, a group, a scope.
//!
//! [`Access`] answers all four from configuration. The application supplies
//! only what is its own: how [`Claims`] become its caller type, which tenants
//! a caller belongs to, its permission rule, and the codes of its refusals.
//!
//! The handler receives a [`Grant`] whose types state what the policy
//! guaranteed: `Grant<User, String>` where a caller and a tenant are required,
//! `Grant<Option<User>, ()>` on a public route without tenancy. Nothing has to
//! be unwrapped, and only a policy can build a `Grant`.
//!
//! # Example
//!
//! ```
//! use davidrs::http::access::{Access, Claims, Grant, Tenancy};
//!
//! /// The application's caller, built from verified claims.
//! struct User {
//!     id: String,
//!     tenants: Vec<String>,
//!     admin: bool,
//! }
//!
//! fn user(claims: &Claims) -> Option<User> {
//!     Some(User {
//!         id: claims.subject()?.to_owned(),
//!         tenants: claims.list("tenants"),
//!         admin: claims.contains("groups", "admin"),
//!     })
//! }
//!
//! let policy: Access<User, User, String> = Access::new(user)
//!     .require_caller()
//!     .tenancy(Tenancy::header("x-tenant-id", |user: &User, tenant| {
//!         user.tenants.iter().any(|own| own == tenant)
//!     }))
//!     .require_tenant()
//!     .permit(|user, _tenant| user.is_some_and(|user| user.admin));
//!
//! /// What a handler behind that policy can rely on.
//! fn handler_view(grant: &Grant<User, String>) -> (&str, &str) {
//!     (grant.caller().id.as_str(), grant.tenant().as_str())
//! }
//! # let _ = (policy, handler_view);
//! ```
//!
//! # Order of checks
//!
//! The order decides which refusal a request with several problems receives,
//! so it is fixed:
//!
//! 1. **Identify.** Gateway claims first; then, when a verifier is
//!    configured, an `Authorization` bearer token. A token that is present but
//!    does not verify, or whose claims do not describe a caller, is refused
//!    with [`AccessErrors::invalid_token`].
//! 2. **Require a caller**, when configured: [`AccessErrors::unauthenticated`].
//! 3. **Select the tenant** from the header. A caller who is not a member of
//!    the requested tenant is refused with [`AccessErrors::forbidden`], and so
//!    is an anonymous request that names one, unless the tenancy is
//!    [`public`](Tenancy::public). Without a header, the tenancy's default for
//!    the caller applies.
//! 4. **Require a tenant**, when configured: [`AccessErrors::tenant_required`].
//! 5. **Permit**, when a rule is configured: [`AccessErrors::forbidden`].
//!
//! # Trust
//!
//! Gateway claims come from the invocation's request context, which API
//! Gateway writes and an HTTP client cannot. Any principal allowed to invoke
//! the function directly (`lambda:InvokeFunction`) can write that context
//! too, so keep invoke permissions as narrow as the authorizer, or turn
//! gateway claims off with [`Access::gateway_claims`] and verify tokens here.
//!
//! A tenant in a [`Grant`] is membership: a caller the policy checked belongs
//! to it. The one exception is a [`public`](Tenancy::public) tenancy, where an
//! anonymous request may name any tenant as context (a storefront, a public
//! catalogue); a handler must not treat that tenant as membership.

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use lambda_http::request::RequestContext;
use lambda_http::RequestExt as _;
use serde_json::{Map, Value};

use super::codes;
use super::failure::{ErrorDefinition, Failure, FailureKind};
use super::policy::Policy;
use super::request::Request;
use super::StatusCode;
use crate::Invocation;

/// Verified claims about a caller, whichever check verified them.
///
/// API Gateway hands claims over in more than one shape: an HTTP API JWT
/// authorizer turns every claim into a string (an array becomes `"[a b]"`), a
/// REST API authorizer keeps JSON. The accessors read both, so an application
/// writes one mapping from claims to its caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claims(Map<String, Value>);

impl Claims {
    /// Wraps a claim set.
    #[must_use]
    pub fn new(claims: Map<String, Value>) -> Self {
        Self(claims)
    }

    /// One claim as JSON.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }

    /// One claim as text, when it is a string.
    #[must_use]
    pub fn string(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(Value::as_str)
    }

    /// The `sub` claim: who the token is about.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.string("sub")
    }

    /// A claim that holds several values, as a list.
    ///
    /// Reads a JSON array, a JSON array written as a string, the `"[a b]"` form
    /// an HTTP API authorizer produces, and space- or comma-separated text such
    /// as an OAuth `scope`. Absent or empty claims give an empty list.
    ///
    /// ```
    /// use davidrs::http::access::Claims;
    ///
    /// let claims = Claims::new(serde_json::from_value(serde_json::json!({
    ///     "groups": "[admin editors]",
    ///     "scope": "orders:read orders:write",
    /// })).unwrap());
    /// assert_eq!(claims.list("groups"), ["admin", "editors"]);
    /// assert_eq!(claims.list("scope"), ["orders:read", "orders:write"]);
    /// ```
    #[must_use]
    pub fn list(&self, name: &str) -> Vec<String> {
        match self.0.get(name) {
            Some(Value::Array(items)) => items.iter().filter_map(item_text).collect(),
            Some(Value::String(text)) => split_list(text),
            _ => Vec::new(),
        }
    }

    /// Whether a list claim (see [`Claims::list`]) contains `value`.
    #[must_use]
    pub fn contains(&self, name: &str, value: &str) -> bool {
        self.list(name).iter().any(|item| item == value)
    }

    /// Every claim.
    #[must_use]
    pub fn all(&self) -> &Map<String, Value> {
        &self.0
    }

    /// The claims an API Gateway authorizer verified and attached to this
    /// request, if any.
    ///
    /// HTTP API: the JWT authorizer's claims, else a Lambda authorizer's context.
    /// REST API (feature `apigw-rest`): a Cognito authorizer's `claims`, else a
    /// Lambda authorizer's context. They come from the request context, which
    /// API Gateway writes and a client cannot; see the module's trust notes.
    ///
    /// Use it where a policy has not run yet, such as a
    /// [`RateLimited`](super::RateLimited) key counting per verified user:
    ///
    /// ```
    /// use davidrs::http::access::Claims;
    /// use davidrs::http::Request;
    ///
    /// fn verified_subject(request: &Request<'_>) -> String {
    ///     Claims::from_gateway(request)
    ///         .and_then(|claims| claims.subject().map(str::to_owned))
    ///         .unwrap_or_else(|| "anonymous".to_owned())
    /// }
    /// # let _ = verified_subject;
    /// ```
    #[must_use]
    pub fn from_gateway(request: &Request<'_>) -> Option<Self> {
        match request.native().request_context_ref()? {
            RequestContext::ApiGatewayV2(context) => {
                let authorizer = context.authorizer.clone()?;
                if let Some(jwt) = authorizer.jwt.filter(|jwt| !jwt.claims.is_empty()) {
                    return Some(Self::new(
                        jwt.claims
                            .into_iter()
                            .map(|(name, value)| (name, Value::String(value)))
                            .collect(),
                    ));
                }
                (!authorizer.fields.is_empty())
                    .then(|| Self::new(authorizer.fields.into_iter().collect()))
            }
            #[cfg(feature = "apigw-rest")]
            RequestContext::ApiGatewayV1(context) => {
                let mut fields = context.authorizer.fields.clone();
                if let Some(Value::Object(claims)) = fields.remove("claims") {
                    return Some(Self::new(claims));
                }
                (!fields.is_empty()).then(|| Self::new(fields.into_iter().collect()))
            }
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

impl From<Map<String, Value>> for Claims {
    fn from(claims: Map<String, Value>) -> Self {
        Self(claims)
    }
}

/// A list element as text: strings as they are, numbers and booleans printed.
fn item_text(item: &Value) -> Option<String> {
    match item {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Splits the text forms of a list claim; see [`Claims::list`].
fn split_list(text: &str) -> Vec<String> {
    let text = text.trim();
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) {
        return items.iter().filter_map(item_text).collect();
    }
    let inner = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(text);
    inner
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The four refusals of an [`Access`] policy.
///
/// The defaults use the codes in [`codes`]; replace any of them to keep the
/// codes an application's clients already branch on. It is deliberately not
/// `#[non_exhaustive]`, so struct-update syntax replaces some refusals and
/// keeps the defaults of the rest:
///
/// ```
/// use davidrs::http::access::AccessErrors;
/// use davidrs::http::{ErrorDefinition, StatusCode};
///
/// let errors = AccessErrors {
///     forbidden: ErrorDefinition::new("ERROR_NOT_YOURS", StatusCode::FORBIDDEN, "Not yours"),
///     ..AccessErrors::default()
/// };
/// assert_eq!(errors.forbidden.code, "ERROR_NOT_YOURS");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessErrors {
    /// A token was presented but is unusable. Default: `401`
    /// [`codes::INVALID_TOKEN`].
    pub invalid_token: ErrorDefinition,
    /// The route requires a caller and none was identified. Default: `401`
    /// [`codes::UNAUTHENTICATED`].
    pub unauthenticated: ErrorDefinition,
    /// The caller may not act for the tenant, or the permission rule refused.
    /// Default: `403` [`codes::FORBIDDEN`].
    pub forbidden: ErrorDefinition,
    /// The route requires a tenant and none was selected. Default: `400`
    /// [`codes::TENANT_REQUIRED`].
    pub tenant_required: ErrorDefinition,
}

impl Default for AccessErrors {
    fn default() -> Self {
        Self {
            invalid_token: ErrorDefinition::new(
                codes::INVALID_TOKEN,
                StatusCode::UNAUTHORIZED,
                "The access token is invalid or expired",
            ),
            unauthenticated: ErrorDefinition::new(
                codes::UNAUTHENTICATED,
                StatusCode::UNAUTHORIZED,
                "Authentication is required",
            ),
            forbidden: ErrorDefinition::new(
                codes::FORBIDDEN,
                StatusCode::FORBIDDEN,
                "Access denied",
            ),
            tenant_required: ErrorDefinition::new(
                codes::TENANT_REQUIRED,
                StatusCode::BAD_REQUEST,
                "A tenant must be selected",
            ),
        }
    }
}

type Member<C> = Arc<dyn Fn(&C, &str) -> bool + Send + Sync>;
type Fallback<C> = Arc<dyn Fn(&C) -> Option<String> + Send + Sync>;

/// How a request selects the tenant it acts for.
pub struct Tenancy<C> {
    header: String,
    member: Member<C>,
    fallback: Option<Fallback<C>>,
    public: bool,
}

impl<C> Tenancy<C> {
    /// The tenant is named by the request header `name`, and `member` says
    /// whether a caller may act for it.
    ///
    /// The header value is trimmed; a blank value counts as absent.
    #[must_use]
    pub fn header(
        name: impl Into<String>,
        member: impl Fn(&C, &str) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            header: name.into(),
            member: Arc::new(member),
            fallback: None,
            public: false,
        }
    }

    /// Lets an anonymous request name a tenant, as public context: the tenant
    /// whose storefront or catalogue it browses.
    ///
    /// Without it, an anonymous request that names a tenant is refused with
    /// [`AccessErrors::forbidden`], so a tenant in a grant always means a
    /// member asked for it. With it, a handler must tell the two cases apart
    /// by whether the grant has a caller.
    #[must_use]
    pub fn public(mut self) -> Self {
        self.public = true;
        self
    }

    /// The tenant of a caller who sends no header — for example the only
    /// tenant they belong to.
    #[must_use]
    pub fn or_else(
        mut self,
        fallback: impl Fn(&C) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.fallback = Some(Arc::new(fallback));
        self
    }

    /// Selects the tenant for this request; see the module's order of checks.
    fn select(
        &self,
        request: &Request<'_>,
        caller: Option<&C>,
        forbidden: ErrorDefinition,
    ) -> Result<Option<String>, Failure> {
        let requested = request
            .header(&self.header)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        match (requested, caller) {
            (Some(tenant), Some(caller)) if !(self.member)(caller, tenant) => {
                Err(refusal(forbidden))
            }
            (Some(_), None) if !self.public => Err(refusal(forbidden)),
            (Some(tenant), _) => Ok(Some(tenant.to_owned())),
            (None, Some(caller)) => {
                Ok(self.fallback.as_ref().and_then(|fallback| fallback(caller)))
            }
            (None, None) => Ok(None),
        }
    }
}

impl<C> fmt::Debug for Tenancy<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tenancy")
            .field("header", &self.header)
            .field("fallback", &self.fallback.is_some())
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

/// How much of something a policy guarantees: required (`T`), optional
/// (`Option<T>`) or not asked for (`()`).
///
/// It is what lets a [`Grant`] carry exactly the guarantee its route was
/// configured with. It is sealed: these three are the only shapes a
/// guarantee has, so no other crate can implement it.
///
/// ```compile_fail,E0277
/// use davidrs::http::access::Requirement;
///
/// struct Perhaps;
///
/// impl Requirement<String> for Perhaps {
///     const REQUIRED: bool = false;
///
///     fn fulfil(_found: Option<String>) -> Option<Self> {
///         Some(Perhaps)
///     }
/// }
/// ```
pub trait Requirement<T>: sealed::Sealed<T> + Sized {
    /// Whether a request without the value is refused.
    const REQUIRED: bool;

    /// Converts what was found, or `None` when a required value is missing.
    fn fulfil(found: Option<T>) -> Option<Self>;
}

impl<T> Requirement<T> for Option<T> {
    const REQUIRED: bool = false;

    fn fulfil(found: Option<T>) -> Option<Self> {
        Some(found)
    }
}

impl<T> Requirement<T> for T {
    const REQUIRED: bool = true;

    fn fulfil(found: Option<T>) -> Option<Self> {
        found
    }
}

impl Requirement<String> for () {
    const REQUIRED: bool = false;

    fn fulfil(_found: Option<String>) -> Option<Self> {
        Some(())
    }
}

/// The supertrait that closes [`Requirement`] to its three shapes.
mod sealed {
    /// Implemented for exactly the types [`Requirement`](super::Requirement)
    /// is; private, so no other crate can add one.
    pub trait Sealed<T> {}

    impl<T> Sealed<T> for Option<T> {}
    impl<T> Sealed<T> for T {}
    impl Sealed<String> for () {}
}

/// What an [`Access`] policy established: the caller and the tenant, typed by
/// what the route requires.
///
/// Its fields are private and only a policy builds one, so a handler that
/// holds a `Grant` knows the checks ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant<Who, Where> {
    caller: Who,
    tenant: Where,
}

impl<Who, Where> Grant<Who, Where> {
    /// The caller: `C`, `Option<C>`, as the route requires.
    pub fn caller(&self) -> &Who {
        &self.caller
    }

    /// The tenant: `String`, `Option<String>` or `()`.
    pub fn tenant(&self) -> &Where {
        &self.tenant
    }

    /// Both parts, owned.
    pub fn into_parts(self) -> (Who, Where) {
        (self.caller, self.tenant)
    }
}

type Identify<C> = Arc<dyn Fn(&Claims) -> Option<C> + Send + Sync>;
type Permit<C> = Arc<dyn Fn(Option<&C>, Option<&str>) -> bool + Send + Sync>;

/// A configurable authentication, tenancy and permission policy.
///
/// `C` is the application's caller type. `Who` and `Where` record what the
/// route requires and become the types of its [`Grant`]; they change as the
/// builder methods are called, starting from an optional caller and no
/// tenancy. See the [module documentation](self) for the order of checks.
pub struct Access<C, Who = Option<C>, Where = ()> {
    identify: Identify<C>,
    gateway: bool,
    #[cfg(feature = "auth")]
    verifier: Option<Arc<crate::auth::Verifier>>,
    tenancy: Option<Tenancy<C>>,
    permit: Option<Permit<C>>,
    errors: AccessErrors,
    guarantees: PhantomData<fn() -> (Who, Where)>,
}

impl<C> Access<C> {
    /// A policy whose callers are built from claims by `identify`.
    ///
    /// `identify` returns `None` for claims that do not describe a caller. By
    /// default the caller is optional, gateway claims are trusted and no
    /// bearer token is verified.
    #[must_use]
    pub fn new(identify: impl Fn(&Claims) -> Option<C> + Send + Sync + 'static) -> Self {
        Self {
            identify: Arc::new(identify),
            gateway: true,
            #[cfg(feature = "auth")]
            verifier: None,
            tenancy: None,
            permit: None,
            errors: AccessErrors::default(),
            guarantees: PhantomData,
        }
    }
}

impl<C, Who, Where> Access<C, Who, Where> {
    /// Whether claims from an API Gateway authorizer identify the caller.
    /// On by default.
    #[must_use]
    pub fn gateway_claims(mut self, trust: bool) -> Self {
        self.gateway = trust;
        self
    }

    /// Also identifies callers by an `Authorization` bearer token, verified
    /// with `verifier`.
    ///
    /// The header may be `Bearer <token>` or the bare token. A value with
    /// another scheme (`Basic …`, a signed-link scheme) is not a caller token
    /// and leaves the request anonymous.
    #[cfg(feature = "auth")]
    #[must_use]
    pub fn verify_bearer(mut self, verifier: Arc<crate::auth::Verifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    /// A permission rule over the caller and the selected tenant, checked
    /// last. Returning `false` refuses with [`AccessErrors::forbidden`].
    ///
    /// The caller is an `Option` even on a route that requires one, so a rule
    /// states explicitly what an anonymous request may do.
    #[must_use]
    pub fn permit(
        mut self,
        rule: impl Fn(Option<&C>, Option<&str>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.permit = Some(Arc::new(rule));
        self
    }

    /// Replaces the refusals, for an application with its own codes.
    #[must_use]
    pub fn errors(mut self, errors: AccessErrors) -> Self {
        self.errors = errors;
        self
    }

    /// The same configuration with other guarantees.
    fn guarantee<W, T>(self) -> Access<C, W, T> {
        Access {
            identify: self.identify,
            gateway: self.gateway,
            #[cfg(feature = "auth")]
            verifier: self.verifier,
            tenancy: self.tenancy,
            permit: self.permit,
            errors: self.errors,
            guarantees: PhantomData,
        }
    }
}

impl<C, Where> Access<C, Option<C>, Where> {
    /// Refuses requests without a caller; the grant then holds a `C`.
    #[must_use]
    pub fn require_caller(self) -> Access<C, C, Where> {
        self.guarantee()
    }
}

impl<C, Who> Access<C, Who, ()> {
    /// Selects a tenant per request; the grant then holds an `Option<String>`.
    #[must_use]
    pub fn tenancy(mut self, tenancy: Tenancy<C>) -> Access<C, Who, Option<String>> {
        self.tenancy = Some(tenancy);
        self.guarantee()
    }
}

impl<C, Who> Access<C, Who, Option<String>> {
    /// Refuses requests without a tenant; the grant then holds a `String`.
    #[must_use]
    pub fn require_tenant(self) -> Access<C, Who, String> {
        self.guarantee()
    }
}

impl<C, Who, Where> Access<C, Who, Where> {
    /// Step 1: the caller, from gateway claims or a verified bearer token.
    async fn identify(&self, request: &Request<'_>) -> Result<Option<C>, Failure> {
        if self.gateway {
            if let Some(caller) =
                Claims::from_gateway(request).and_then(|claims| (self.identify)(&claims))
            {
                return Ok(Some(caller));
            }
        }
        #[cfg(feature = "auth")]
        if let Some(verifier) = &self.verifier {
            if let Some(token) = request.header("authorization").and_then(bearer_token) {
                let verified = verifier
                    .verify(token)
                    .await
                    .map_err(|_| refusal(self.errors.invalid_token))?;
                let claims = Claims::new(verified.all().clone());
                return (self.identify)(&claims)
                    .map(Some)
                    .ok_or_else(|| refusal(self.errors.invalid_token));
            }
        }
        Ok(None)
    }
}

impl<C, Who, Where> Policy for Access<C, Who, Where>
where
    C: Send + 'static,
    Who: Requirement<C> + Send + 'static,
    Where: Requirement<String> + Send + 'static,
{
    type Scope = Grant<Who, Where>;

    async fn authorize(
        &self,
        request: &Request<'_>,
        _invocation: &Invocation,
    ) -> Result<Self::Scope, Failure> {
        let caller = self.identify(request).await?;
        if Who::REQUIRED && caller.is_none() {
            return Err(refusal(self.errors.unauthenticated));
        }
        let tenant = match &self.tenancy {
            Some(tenancy) => tenancy.select(request, caller.as_ref(), self.errors.forbidden)?,
            None => None,
        };
        if Where::REQUIRED && tenant.is_none() {
            return Err(refusal(self.errors.tenant_required));
        }
        if let Some(permit) = &self.permit {
            if !permit(caller.as_ref(), tenant.as_deref()) {
                return Err(refusal(self.errors.forbidden));
            }
        }
        let caller = Who::fulfil(caller).ok_or_else(|| refusal(self.errors.unauthenticated))?;
        let tenant = Where::fulfil(tenant).ok_or_else(|| refusal(self.errors.tenant_required))?;
        Ok(Grant { caller, tenant })
    }
}

impl<C, Who, Where> fmt::Debug for Access<C, Who, Where> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("Access");
        debug.field("gateway_claims", &self.gateway);
        #[cfg(feature = "auth")]
        debug.field("verify_bearer", &self.verifier.is_some());
        debug
            .field("tenancy", &self.tenancy)
            .field("permit", &self.permit.is_some())
            .field("errors", &self.errors)
            .finish_non_exhaustive()
    }
}

/// A refusal of the policy stage.
fn refusal(definition: ErrorDefinition) -> Failure {
    definition.failure().with_kind(FailureKind::Policy)
}

/// The caller token in an `Authorization` value: `Bearer <token>` or a bare
/// token. Any other scheme, or the `Bearer` scheme with no token, is not a
/// caller token.
#[cfg(feature = "auth")]
fn bearer_token(value: &str) -> Option<&str> {
    let value = value.trim();
    match value.split_once(char::is_whitespace) {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => {
            Some(token.trim()).filter(|token| !token.is_empty())
        }
        Some(_) => None,
        None if value.eq_ignore_ascii_case("bearer") => None,
        None => Some(value).filter(|token| !token.is_empty()),
    }
}
