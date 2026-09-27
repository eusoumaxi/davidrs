//! An MCP server from one Lambda function: tools an AI client lists and
//! calls, over Streamable HTTP.
//!
//! [`Server`] answers the Model Context Protocol with one JSON object per
//! request and keeps no session, so any instance serves any request. It
//! speaks MCP revision [`PROTOCOL_VERSION`]. It runs on the buffered HTTP
//! pipeline ([`Api`]): the policy you choose authenticates every `POST`, and a
//! refused caller receives the `401` that points an OAuth client at the
//! [`ProtectedResource`] metadata.
//!
//! Tools come from two places: [`Tool::new`] with an async handler, and, with
//! the `mcp-openapi` feature, the operations of an OpenAPI document
//! (`mcp::openapi`), each call forwarded to your API with the caller's own
//! token.
//!
//! # Examples
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use davidrs::http::{Failure, Public};
//! use davidrs::mcp::{Server, Tool};
//! use davidrs::{Context, RuntimeError};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct Echo {
//!     text: String,
//! }
//!
//! async fn echo(_app: Arc<()>, input: Echo, _context: Context<()>) -> Result<String, Failure> {
//!     Ok(input.text)
//! }
//!
//! # async fn run() -> Result<(), RuntimeError> {
//! let schema = serde_json::json!({
//!     "type": "object",
//!     "properties": { "text": { "type": "string" } },
//!     "required": ["text"]
//! });
//! Server::new("echo", "1.0.0", Public)
//!     .tool(Tool::new("echo", "Repeats the text", schema, echo))
//!     .run(Arc::new(()))
//!     .await
//! # }
//! ```
//!
//! # What a request goes through
//!
//! 1. **Admission.** A request with an `Origin` outside
//!    [`Server::allow_origins`] is refused with `403`, then
//!    [`Server::admission`] counts it; both run before anything is parsed.
//! 2. **Route.** `GET /.well-known/oauth-protected-resource` (and the same
//!    path followed by the resource's own path) serves the metadata; any
//!    other `POST` is the MCP endpoint; everything else is `405`.
//! 3. **Policy**, for the endpoint only: its `401` carries
//!    `WWW-Authenticate: Bearer resource_metadata="…"`.
//! 4. **Protocol.** One JSON-RPC message whose metadata and mirrored headers
//!    name revision [`PROTOCOL_VERSION`]; any other revision gets `-32022`.
//!    The methods are `server/discover`, `ping`, `tools/list` and
//!    `tools/call`. A notification gets `202`; any other method gets `404`
//!    with `-32601`.
//! 5. **Tool.** The handler runs under the invocation deadline less
//!    [`CALL_MARGIN`], so a slow tool still returns a well-formed result.

mod protocol;
mod tool;

#[cfg(feature = "mcp-openapi")]
pub mod openapi;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use lambda_http::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use serde_json::{json, Map, Value};

pub use protocol::PROTOCOL_VERSION;
pub use tool::{Tool, INVALID_ARGUMENTS};

use self::protocol::{Message, Reply};
use crate::http::{
    codes, literal, Admission, AdmitAll, Api, ErrorRenderer, Failure, HttpResponse, Policy,
    Request, DEFAULT_BODY_LIMIT,
};
use crate::{Context, Invocation, RuntimeError};

/// The most tools one server holds.
///
/// Every tool's definition is sent in each `tools/list` answer and usually
/// reaches the model's context, so a long list costs on every turn and makes
/// the model choose worse. Split a larger API into several servers, or select
/// fewer operations.
pub const MAX_TOOLS: usize = 128;

/// Time kept back from the invocation deadline when a tool runs, so a tool
/// that runs out of time still returns a result instead of a `504`.
pub const CALL_MARGIN: Duration = Duration::from_secs(1);

/// How long a client may reuse a `tools/list` or `server/discover` answer.
const CACHE_TTL_MS: u64 = 300_000;

/// The path an OAuth client reads the protected-resource metadata from.
const METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

type Visible<Scope> = Box<dyn Fn(&str, &Scope) -> bool + Send + Sync>;

/// An MCP server: its identity, its policy and its tools.
///
/// Build it once in `main`, then [`Server::run`] it in a Lambda, or
/// [`Server::handle`] one request in a test. `P` is any HTTP [`Policy`];
/// its scope is what every tool handler receives. `A` is the [`Admission`]
/// that counts requests before they are parsed, such as a
/// [`RateLimited`](crate::http::RateLimited).
pub struct Server<App, P: Policy, A = AdmitAll> {
    name: &'static str,
    version: &'static str,
    policy: Arc<P>,
    admission: Arc<A>,
    instructions: Option<String>,
    origins: Arc<[String]>,
    resource: Option<ProtectedResource>,
    body_limit: usize,
    tools: Vec<Tool<App, P::Scope>>,
    visible: Option<Visible<P::Scope>>,
}

impl<App, P> Server<App, P>
where
    App: Send + Sync + 'static,
    P: Policy,
{
    /// A server named `name` at `version`, whose endpoint is authorized by
    /// `policy` and which admits every request. Performs no I/O.
    ///
    /// The name and version are what clients display. Use [`Public`] for a
    /// server without authentication, or an
    /// [`Access`](crate::http::access::Access) policy.
    ///
    /// [`Public`]: crate::http::Public
    #[must_use]
    pub fn new(name: &'static str, version: &'static str, policy: P) -> Self {
        Self {
            name,
            version,
            policy: Arc::new(policy),
            admission: Arc::new(AdmitAll),
            instructions: None,
            origins: Arc::from([]),
            resource: None,
            body_limit: DEFAULT_BODY_LIMIT,
            tools: Vec::new(),
            visible: None,
        }
    }
}

impl<App, P, A> Server<App, P, A>
where
    App: Send + Sync + 'static,
    P: Policy,
    A: Admission,
{
    /// Counts every request with `admission` before it is parsed, after the
    /// origin check; its refusal, such as a `429`, keeps its headers.
    ///
    /// The MCP specification asks servers to rate limit tool calls: a
    /// [`RateLimited`](crate::http::RateLimited) keyed on the caller's
    /// address, or on a hash of its `Authorization` header, does.
    #[must_use]
    pub fn admission<B: Admission>(self, admission: B) -> Server<App, P, B> {
        Server {
            name: self.name,
            version: self.version,
            policy: self.policy,
            admission: Arc::new(admission),
            instructions: self.instructions,
            origins: self.origins,
            resource: self.resource,
            body_limit: self.body_limit,
            tools: self.tools,
            visible: self.visible,
        }
    }

    /// Guidance a client may add to the model's context: what the server is
    /// for and how its tools fit together.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// The browser origins allowed to call the server, such as
    /// `https://app.example.com`.
    ///
    /// Non-browser clients send no `Origin` and are not affected. A request
    /// from any origin not listed is refused, which is what protects a server
    /// from a web page the user happens to have open. None are allowed by
    /// default.
    #[must_use]
    pub fn allow_origins(mut self, origins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.origins = origins.into_iter().map(Into::into).collect();
        self
    }

    /// Publishes the RFC 9728 metadata that tells an OAuth client where to
    /// obtain a token, and points every `401` at it.
    #[must_use]
    pub fn protected_resource(mut self, resource: ProtectedResource) -> Self {
        self.resource = Some(resource);
        self
    }

    /// Caps the request body, in bytes; [`DEFAULT_BODY_LIMIT`] by default.
    ///
    /// A `tools/call` body is almost entirely the tool's arguments, so this
    /// is also the bound on arguments.
    #[must_use]
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.body_limit = bytes;
        self
    }

    /// Adds a tool.
    ///
    /// # Panics
    ///
    /// Panics when the name is not 1 to 128 ASCII letters, digits, `_`, `-`
    /// or `.`, when another tool already has it, or when the server already
    /// holds [`MAX_TOOLS`]. These are mistakes in the program, found on the
    /// first cold start.
    #[must_use]
    pub fn tool(mut self, tool: Tool<App, P::Scope>) -> Self {
        let name = tool.name();
        assert!(valid_name(name), "invalid tool name {name:?}");
        assert!(
            self.tools.iter().all(|other| other.name() != name),
            "duplicate tool name {name:?}"
        );
        assert!(self.tools.len() < MAX_TOOLS, "more than {MAX_TOOLS} tools");
        self.tools.push(tool);
        self
    }

    /// Shows a tool to a caller only when `visible(tool_name, scope)` holds.
    ///
    /// A hidden tool is left out of `tools/list` and answered as unknown by
    /// `tools/call`, so a caller cannot use a tool it cannot see. With a rule
    /// the list differs by caller, so clients are told not to share it
    /// (`cacheScope: "private"`); without one, every caller sees every tool.
    #[must_use]
    pub fn visible(
        mut self,
        visible: impl Fn(&str, &P::Scope) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.visible = Some(Box::new(visible));
        self
    }

    /// Serves one request and returns the response.
    ///
    /// Failures are rendered, so this always yields a response. [`Server::run`]
    /// calls it for every invocation; tests call it with a hand-built request.
    pub async fn handle(&self, app: Arc<App>, request: lambda_http::Request) -> HttpResponse {
        let serve = |app, inbound, context| self.serve(app, inbound, context);
        self.api().handle(app, request, &decode, &serve).await
    }

    /// Runs the server in the Lambda loop until the runtime stops.
    ///
    /// # Errors
    ///
    /// Returns a [`RuntimeError`] only when the runtime itself fails.
    pub async fn run(self, app: Arc<App>) -> Result<(), RuntimeError> {
        let server = &self;
        self.api()
            .run(app, decode, move |app, inbound, context| {
                server.serve(app, inbound, context)
            })
            .await
    }

    /// The HTTP pipeline this server runs on.
    fn api(&self) -> Api<Gate<P>, RpcErrors, Origins<A>> {
        let challenge = self
            .resource
            .as_ref()
            .map_or_else(|| "Bearer".to_owned(), ProtectedResource::challenge);
        Api::new(
            self.name,
            Gate(Arc::clone(&self.policy)),
            RpcErrors { challenge },
        )
        .admission(Origins {
            allowed: Arc::clone(&self.origins),
            then: Arc::clone(&self.admission),
        })
        .body_limit(self.body_limit)
    }

    /// The handler the pipeline calls once the gate has routed the request.
    async fn serve(
        &self,
        app: Arc<App>,
        inbound: Inbound,
        context: Context<Target<P::Scope>>,
    ) -> Result<HttpResponse, Failure> {
        let invocation = context.invocation().clone();
        match context.into_scope() {
            Target::Endpoint(scope) => Ok(self.endpoint(app, inbound, invocation, scope).await),
            Target::Metadata => match &self.resource {
                Some(resource) => Ok(literal(
                    StatusCode::OK,
                    "application/json",
                    resource.document().to_string(),
                )),
                None => Err(Failure::new(
                    StatusCode::NOT_FOUND,
                    codes::NOT_FOUND,
                    "Not found",
                )),
            },
            Target::Elsewhere => Err(Failure::new(
                StatusCode::METHOD_NOT_ALLOWED,
                codes::METHOD_NOT_ALLOWED,
                "The MCP endpoint accepts POST",
            )
            .with_header("allow", "POST")),
        }
    }

    /// One JSON-RPC message from an authorized caller.
    async fn endpoint(
        &self,
        app: Arc<App>,
        inbound: Inbound,
        invocation: Invocation,
        scope: P::Scope,
    ) -> HttpResponse {
        let message = match protocol::parse(&inbound.body) {
            Ok(message) => message,
            Err(reply) => return reply.into_response(),
        };
        let Some(id) = message.id.clone() else {
            return Reply::accepted().into_response();
        };
        if let Err(reply) = protocol::validate(&inbound.headers, &message) {
            return reply.into_response();
        }
        let authorization = inbound
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        self.answer(app, message, id, invocation, scope, authorization)
            .await
            .into_response()
    }

    /// Answers one request by method.
    async fn answer(
        &self,
        app: Arc<App>,
        mut message: Message,
        id: Value,
        invocation: Invocation,
        scope: P::Scope,
        authorization: Option<String>,
    ) -> Reply {
        let server = json!({ "name": self.name, "version": self.version });
        let method = std::mem::take(&mut message.method);
        let mut result = Map::new();
        match method.as_str() {
            "server/discover" => {
                result.insert("supportedVersions".into(), json!([PROTOCOL_VERSION]));
                result.insert("capabilities".into(), json!({ "tools": {} }));
                if let Some(instructions) = &self.instructions {
                    result.insert("instructions".into(), json!(instructions));
                }
                result.insert("ttlMs".into(), json!(CACHE_TTL_MS));
                result.insert("cacheScope".into(), json!("public"));
            }
            "ping" => {}
            "tools/list" => {
                let tools: Vec<&Value> = self
                    .tools
                    .iter()
                    .filter(|tool| self.shows(tool, &scope))
                    .map(|tool| &tool.definition)
                    .collect();
                let shared = if self.visible.is_some() {
                    "private"
                } else {
                    "public"
                };
                result.insert("tools".into(), json!(tools));
                result.insert("ttlMs".into(), json!(CACHE_TTL_MS));
                result.insert("cacheScope".into(), json!(shared));
            }
            "tools/call" => {
                match self
                    .call(app, message, &id, invocation, scope, authorization)
                    .await
                {
                    Ok(called) => result = called,
                    Err(reply) => return reply,
                }
            }
            method => {
                return Reply::error(
                    StatusCode::NOT_FOUND,
                    Some(id),
                    protocol::METHOD_NOT_FOUND,
                    format!("Method not found: {method}"),
                    None,
                );
            }
        }
        Reply::result(id, &server, result)
    }

    /// Whether the caller may see and call `tool`.
    fn shows(&self, tool: &Tool<App, P::Scope>, scope: &P::Scope) -> bool {
        self.visible
            .as_ref()
            .is_none_or(|visible| visible(tool.name(), scope))
    }

    /// Runs one tool under the invocation deadline less [`CALL_MARGIN`].
    ///
    /// # Errors
    ///
    /// A tool the caller cannot see is unknown (`200`), and arguments that
    /// are not an object are malformed (`400`). Both are `-32602`, as the
    /// specification lists them.
    async fn call(
        &self,
        app: Arc<App>,
        mut message: Message,
        id: &Value,
        mut invocation: Invocation,
        scope: P::Scope,
        authorization: Option<String>,
    ) -> Result<Map<String, Value>, Reply> {
        let invalid = |status, text: &str| {
            Reply::error(
                status,
                Some(id.clone()),
                protocol::INVALID_PARAMS,
                text,
                None,
            )
        };
        let name = message.text("name").unwrap_or_default().to_owned();
        let Some(tool) = self
            .tools
            .iter()
            .find(|tool| tool.name() == name && self.shows(tool, &scope))
        else {
            return Err(invalid(StatusCode::OK, &format!("Unknown tool: {name}")));
        };
        let arguments = match message.params.remove("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(arguments)) => arguments,
            Some(_) => {
                return Err(invalid(
                    StatusCode::BAD_REQUEST,
                    "params.arguments must be an object",
                ))
            }
        };
        let deadline = invocation.deadline.with_margin(CALL_MARGIN);
        invocation.deadline = deadline;
        let work = (tool.run)(
            app,
            arguments,
            Context::new(invocation, scope),
            authorization,
        );
        Ok(tool::result(&name, deadline.run(work).await))
    }
}

impl<App, P: Policy, A> fmt::Debug for Server<App, P, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("origins", &self.origins)
            .field("resource", &self.resource)
            .field("body_limit", &self.body_limit)
            .field("tools", &self.tools)
            .field("visible", &self.visible.is_some())
            .finish_non_exhaustive()
    }
}

/// Whether `name` is a tool name every client accepts: 1 to 128 ASCII
/// letters, digits, `_`, `-` and `.`.
fn valid_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// The RFC 9728 metadata of a protected MCP server: which resource it is,
/// and which authorization servers issue tokens for it.
///
/// A client that receives a `401` reads this document, discovers the
/// authorization server's own metadata from its issuer (RFC 8414 or OpenID
/// Connect discovery), and runs the OAuth authorization-code flow with PKCE,
/// asking for a token for `resource` (RFC 8707).
///
/// # Examples
///
/// ```
/// use davidrs::mcp::ProtectedResource;
///
/// let resource = ProtectedResource::new("https://mcp.example.com", ["https://id.example.com"])
///     .scopes(["orders:read"]);
/// # let _ = resource;
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedResource {
    resource: String,
    authorization_servers: Vec<String>,
    scopes: Vec<String>,
}

impl ProtectedResource {
    /// The server's canonical URI, exactly as clients are configured with it
    /// (`https://mcp.example.com`, or `https://example.com/mcp`), and the
    /// issuers whose tokens it accepts.
    #[must_use]
    pub fn new(
        resource: impl Into<String>,
        authorization_servers: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            resource: resource.into(),
            authorization_servers: authorization_servers.into_iter().map(Into::into).collect(),
            scopes: Vec::new(),
        }
    }

    /// The scopes a client should ask for: published as `scopes_supported`
    /// and in the `401` challenge.
    #[must_use]
    pub fn scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// The metadata document.
    fn document(&self) -> Value {
        let mut document = json!({
            "resource": self.resource,
            "authorization_servers": self.authorization_servers,
            "bearer_methods_supported": ["header"],
        });
        if !self.scopes.is_empty() {
            document["scopes_supported"] = json!(self.scopes);
        }
        document
    }

    /// Where the document is published: the well-known path inserted between
    /// the resource's host and its path (RFC 9728, section 3.1).
    fn metadata_url(&self) -> String {
        let resource = self.resource.trim_end_matches('/');
        let host = resource.find("://").map_or(0, |scheme| scheme + 3);
        let path = resource[host..]
            .find('/')
            .map_or(resource.len(), |at| host + at);
        format!("{}{METADATA_PATH}{}", &resource[..path], &resource[path..])
    }

    /// The `WWW-Authenticate` value of a `401`.
    fn challenge(&self) -> String {
        let mut challenge = format!("Bearer resource_metadata=\"{}\"", self.metadata_url());
        if !self.scopes.is_empty() {
            challenge.push_str(&format!(", scope=\"{}\"", self.scopes.join(" ")));
        }
        challenge
    }
}

/// The request as the protocol needs it: its headers and its bytes.
struct Inbound {
    headers: HeaderMap,
    body: Vec<u8>,
}

/// Keeps the request whole; the body limit was checked before.
fn decode(request: &Request<'_>) -> Result<Inbound, Failure> {
    Ok(Inbound {
        headers: request.headers().clone(),
        body: request.raw_body().to_vec(),
    })
}

/// What a request is for, decided by the [`Gate`].
enum Target<Scope> {
    /// A `POST`: the MCP endpoint, for a caller the policy authorized.
    Endpoint(Scope),
    /// `GET` of the protected-resource metadata, which is public.
    Metadata,
    /// Anything else: `405`.
    Elsewhere,
}

/// The server's policy: the application's policy for the endpoint, and no
/// authentication for the metadata a client needs before it has a token.
struct Gate<P>(Arc<P>);

impl<P: Policy> Policy for Gate<P> {
    type Scope = Target<P::Scope>;

    async fn authorize(
        &self,
        request: &Request<'_>,
        invocation: &Invocation,
    ) -> Result<Self::Scope, Failure> {
        let path = request.native().uri().path();
        match *request.method() {
            Method::POST => self
                .0
                .authorize(request, invocation)
                .await
                .map(Target::Endpoint),
            Method::GET if is_metadata(path) => Ok(Target::Metadata),
            _ => Ok(Target::Elsewhere),
        }
    }
}

/// Whether `path` is the metadata's: the well-known path, alone or followed
/// by the resource's own path.
fn is_metadata(path: &str) -> bool {
    path.strip_prefix(METADATA_PATH)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Refuses a browser origin outside the allowlist, before anything is
/// parsed, then runs the server's own admission.
struct Origins<A> {
    allowed: Arc<[String]>,
    then: Arc<A>,
}

impl<A: Admission> Admission for Origins<A> {
    async fn check(
        &self,
        request: &Request<'_>,
        invocation: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        match request.header("origin") {
            Some(origin) if !self.allowed.iter().any(|allowed| allowed == origin) => {
                Err(Failure::new(
                    StatusCode::FORBIDDEN,
                    codes::ORIGIN_NOT_ALLOWED,
                    "Origin not allowed",
                ))
            }
            _ => self.then.check(request, invocation).await,
        }
    }
}

/// Renders the pipeline's own failures as JSON-RPC errors without an `id`,
/// and adds the OAuth challenge to a `401`.
struct RpcErrors {
    challenge: String,
}

impl ErrorRenderer for RpcErrors {
    fn render(&self, failure: &Failure) -> HttpResponse {
        let code = if failure.is_server_error() {
            protocol::INTERNAL_ERROR
        } else {
            protocol::INVALID_REQUEST
        };
        let body = json!({
            "jsonrpc": "2.0",
            "error": { "code": code, "message": failure.public_message() }
        });
        let mut response = literal(failure.status(), "application/json", body.to_string());
        for (name, value) in failure.headers() {
            response.headers_mut().insert(name.clone(), value.clone());
        }
        if failure.status() == StatusCode::UNAUTHORIZED {
            let challenge = HeaderValue::try_from(self.challenge.as_str())
                .unwrap_or(HeaderValue::from_static("Bearer"));
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, challenge);
        }
        response
    }
}
