//! Tools from an OpenAPI 3 document: each selected operation is a tool, and
//! each call is one request to the API.
//!
//! A call carries no credential until you choose one:
//!
//! - [`OpenApi::forward_caller_token`] sends the caller's own
//!   `Authorization` header, so the API authorizes the call exactly as it
//!   would a request from the application and the server adds no permission
//!   model of its own. Only for your own API, which accepts tokens issued for
//!   this server: the MCP specification forbids passing a token through to
//!   an API it was not issued for.
//! - [`OpenApi::header`] sends a fixed header of the tool's own, such as a
//!   service key, with the user's identity carried as data the API trusts
//!   from this client alone.
//!
//! # From an operation to a tool
//!
//! - **Name**: the `operationId`, with every character outside ASCII
//!   letters, digits, `_`, `-` and `.` replaced by `_`, cut to 128
//!   characters. An operation without one is not a tool.
//! - **Description**: `description`, else `summary`; `summary` is also the
//!   `title`.
//! - **Arguments**: one object holding the path, query and header parameters
//!   (path-level ones included) and the properties of a JSON body. A body
//!   that is not an object, or whose properties share a name with a
//!   parameter, is one `body` argument instead. Local `$ref`s are inlined, so
//!   every schema stands alone, with the keywords beside a `$ref` applying
//!   together with its target. An operation whose body is not JSON is not a
//!   tool. A `null` argument counts as absent, except a field of a flattened
//!   body, which is sent as `null`. Cookie parameters and the headers the
//!   transport sets itself (`Accept`, `Content-Type`, `Authorization`,
//!   `Host`, `Content-Length`, `Transfer-Encoding`) are never arguments.
//! - **URL**: the base URL followed by the path, with path values
//!   percent-encoded and query values encoded as `name=value` pairs. A call
//!   therefore always reaches the base URL's own host; a document whose path
//!   does not start with `/` is refused when the document is read.
//! - **Hints**: `GET`, `HEAD`, `OPTIONS` and `TRACE` are read-only; other
//!   methods may be destructive, and the idempotent methods say so.
//!
//! # Examples
//!
//! ```
//! use davidrs::client::{self, Limits};
//! use davidrs::mcp::openapi::OpenApi;
//!
//! let document = serde_json::json!({
//!     "openapi": "3.1.0",
//!     "paths": {
//!         "/orders/{id}": {
//!             "get": {
//!                 "operationId": "getOrder",
//!                 "summary": "Read one order",
//!                 "parameters": [{ "name": "id", "in": "path", "schema": { "type": "string" } }]
//!             },
//!             "delete": { "operationId": "deleteOrder" }
//!         }
//!     }
//! });
//! let api = OpenApi::new(&document, "https://api.example.com", client::build(Limits::default())?)?
//!     .select(|operation| operation.method() == "GET");
//! assert_eq!(api.operations().len(), 1);
//! assert_eq!(api.operations()[0].name(), "getOrder");
//! # Ok::<(), davidrs::RuntimeError>(())
//! ```

use std::fmt;
use std::sync::Arc;

use lambda_http::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use serde_json::{Map, Value, json};

use super::Server;
use super::tool::{INVALID_ARGUMENTS, Run, Tool};
use crate::RuntimeError;
use crate::http::{Admission, Failure, Policy, codes};

/// The largest API answer a tool result carries, by default (1 MiB).
pub const DEFAULT_RESPONSE_LIMIT: usize = 1024 * 1024;

/// The code of a call the API answered with a 4xx; the result carries the
/// API's status and body, which the caller could have read directly.
pub const UPSTREAM_REFUSED: &str = "ERROR_UPSTREAM_REFUSED";

/// The code of a call the API failed or never answered: a 5xx, a redirect,
/// or a transport error. Nothing about the failure reaches the caller.
pub const UPSTREAM_UNAVAILABLE: &str = "ERROR_UPSTREAM_UNAVAILABLE";

/// How many schema nodes one operation's inlining may visit; past it, the
/// remaining `$ref`s stay as they are.
const SCHEMA_BUDGET: usize = 10_000;

/// The methods an OpenAPI path item holds, by their key.
const METHODS: [(&str, Method); 8] = [
    ("get", Method::GET),
    ("put", Method::PUT),
    ("post", Method::POST),
    ("delete", Method::DELETE),
    ("options", Method::OPTIONS),
    ("head", Method::HEAD),
    ("patch", Method::PATCH),
    ("trace", Method::TRACE),
];

/// The operations of an OpenAPI document, and the API their calls go to.
///
/// Add them to a server with [`Server::openapi`].
pub struct OpenApi {
    operations: Vec<Operation>,
    base_url: String,
    http: reqwest::Client,
    response_limit: usize,
    forward_caller_token: bool,
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl OpenApi {
    /// Reads the operations of `document` that can be tools, to be called at
    /// `base_url` with `http`.
    ///
    /// `base_url` is the API's root, such as `https://api.example.com/v1`: an
    /// operation's path is appended to it. The document's `servers` are
    /// ignored, so one document serves every environment. Build `http` with
    /// [`client::build`](crate::client::build): its limits bound each call,
    /// besides the invocation deadline.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Configuration`] when `document` is not an
    /// OpenAPI 3 document, or when a path does not start with `/`: appended
    /// to the base URL, such a path could name another host (`@other.example`)
    /// and send the caller's token there.
    pub fn new(
        document: &Value,
        base_url: impl Into<String>,
        http: reqwest::Client,
    ) -> Result<Self, RuntimeError> {
        let version = document["openapi"].as_str().unwrap_or_default();
        if !version.starts_with("3.") {
            return Err(RuntimeError::Configuration(
                "the tools document is not OpenAPI 3".to_owned(),
            ));
        }
        let mut operations = Vec::new();
        for (path, item) in document["paths"].as_object().into_iter().flatten() {
            if !path.starts_with('/') {
                return Err(RuntimeError::Configuration(format!(
                    "the tools document has a path that does not start with /: {path}"
                )));
            }
            for (key, method) in &METHODS {
                if let Some(operation) = derive(document, path, item, key, method) {
                    operations.push(operation);
                }
            }
        }
        Ok(Self {
            operations,
            base_url: base_url.into(),
            http,
            response_limit: DEFAULT_RESPONSE_LIMIT,
            forward_caller_token: false,
            headers: Vec::new(),
        })
    }

    /// Sends the caller's own `Authorization` header with every call.
    ///
    /// Use it only when the base URL is your own API and that API accepts
    /// tokens issued for this MCP server — same owner, same authorization
    /// server, this server's resource among its audiences. Anywhere else a
    /// forwarded token is a confused deputy: the API would act on a token
    /// that was never issued for it.
    #[must_use]
    pub fn forward_caller_token(mut self) -> Self {
        self.forward_caller_token = true;
        self
    }

    /// Sends a fixed header with every call — the credential this server
    /// holds for the API, such as a service key.
    ///
    /// The value is marked sensitive, so it never appears in `Debug` output.
    #[must_use]
    pub fn header(mut self, name: HeaderName, mut value: HeaderValue) -> Self {
        value.set_sensitive(true);
        self.headers.push((name, value));
        self
    }

    /// Keeps only the operations for which `keep` holds.
    ///
    /// Select deliberately: every tool costs the model context on each turn,
    /// and a server holds at most [`MAX_TOOLS`](super::MAX_TOOLS).
    #[must_use]
    pub fn select(mut self, keep: impl Fn(&Operation) -> bool) -> Self {
        self.operations.retain(|operation| keep(operation));
        self
    }

    /// Caps the API answer a call reads, in bytes; larger answers become a
    /// failed call ([`codes::LIMIT_EXCEEDED`]).
    #[must_use]
    pub fn response_limit(mut self, bytes: usize) -> Self {
        self.response_limit = bytes;
        self
    }

    /// The operations that will become tools: path by path and, within a
    /// path, in the order `get`, `put`, `post`, `delete`, `options`, `head`,
    /// `patch`, `trace`.
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }
}

impl fmt::Debug for OpenApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenApi")
            .field("operations", &self.operations)
            .field("response_limit", &self.response_limit)
            .field("forward_caller_token", &self.forward_caller_token)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl<App, P, A> Server<App, P, A>
where
    App: Send + Sync + 'static,
    P: Policy,
    A: Admission,
{
    /// Adds a tool for each operation `api` selected.
    ///
    /// # Panics
    ///
    /// As [`Server::tool`]: when two tools share a name, or the server would
    /// hold more than [`MAX_TOOLS`](super::MAX_TOOLS).
    #[must_use]
    pub fn openapi(mut self, api: OpenApi) -> Self {
        let upstream = Arc::new(Upstream {
            base_url: api.base_url,
            http: api.http,
            limit: api.response_limit,
            forward_caller_token: api.forward_caller_token,
            headers: api.headers,
        });
        for operation in api.operations {
            let definition = operation.tool.clone();
            let operation = Arc::new(operation);
            let upstream = Arc::clone(&upstream);
            let run: Run<App, P::Scope> =
                Box::new(move |_app, arguments, _context, authorization| {
                    Box::pin(forward(
                        Arc::clone(&operation),
                        Arc::clone(&upstream),
                        arguments,
                        authorization,
                    ))
                });
            self = self.tool(Tool::from_parts(definition, run));
        }
        self
    }
}

/// One operation of the document, and the tool it becomes.
#[derive(Debug, PartialEq, Eq)]
pub struct Operation {
    name: String,
    method: Method,
    path: String,
    definition: Value,
    parameters: Vec<Parameter>,
    body: Body,
    required: Vec<String>,
    tool: Value,
}

impl Operation {
    /// The tool's name, derived from the `operationId`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The HTTP method, uppercase: `"GET"`.
    pub fn method(&self) -> &str {
        self.method.as_str()
    }

    /// The path template, such as `/orders/{id}`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the operation lists `tag` in its `tags`.
    pub fn has_tag(&self, tag: &str) -> bool {
        self.definition["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|own| own == tag))
    }

    /// The operation object as the document wrote it, for selecting on an
    /// extension such as `x-internal`.
    pub fn definition(&self) -> &Value {
        &self.definition
    }
}

/// Where a parameter travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Path,
    Query,
    Header,
}

#[derive(Debug, PartialEq, Eq)]
struct Parameter {
    name: String,
    location: Location,
}

/// How the body is written in the arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    /// The operation takes no JSON body.
    None,
    /// The body's properties are arguments of their own.
    Fields,
    /// The body is the `body` argument.
    Whole,
}

/// The API a server's operations call.
struct Upstream {
    base_url: String,
    http: reqwest::Client,
    limit: usize,
    forward_caller_token: bool,
    headers: Vec<(HeaderName, HeaderValue)>,
}

/// The operation at `path` under `key`, when it is a tool.
fn derive(
    document: &Value,
    path: &str,
    item: &Value,
    key: &str,
    method: &Method,
) -> Option<Operation> {
    let definition = item.get(key)?;
    let name = tool_name(definition["operationId"].as_str()?)?;
    let mut budget = SCHEMA_BUDGET;
    let mut resolve = |node: &Value| inline(node, document, &mut Vec::new(), &mut budget);

    let mut declared: Vec<Value> = Vec::new();
    for parameter in [&item["parameters"], &definition["parameters"]]
        .into_iter()
        .filter_map(Value::as_array)
        .flatten()
    {
        let parameter = resolve(parameter);
        declared
            .retain(|other| other["name"] != parameter["name"] || other["in"] != parameter["in"]);
        declared.push(parameter);
    }
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut parameters = Vec::new();
    for parameter in declared {
        let (Some(pname), Some(location)) = (parameter["name"].as_str(), location(&parameter))
        else {
            continue;
        };
        let mut schema = parameter
            .get("schema")
            .cloned()
            .unwrap_or(json!({ "type": "string" }));
        if let (Some(schema), Some(description)) =
            (schema.as_object_mut(), parameter.get("description"))
        {
            schema.insert("description".into(), description.clone());
        }
        if location == Location::Path || parameter["required"] == true {
            required.push(pname.to_owned());
        }
        properties.insert(pname.to_owned(), schema);
        parameters.push(Parameter {
            name: pname.to_owned(),
            location,
        });
    }

    let request = resolve(&definition["requestBody"]);
    let body = match request["content"].as_object() {
        None => Body::None,
        Some(content) => {
            let schema = content
                .get("application/json")?
                .get("schema")
                .cloned()
                .unwrap_or(json!({}));
            let fields = schema["properties"].as_object().filter(|fields| {
                schema["type"] == "object" && fields.keys().all(|key| !properties.contains_key(key))
            });
            match fields {
                Some(fields) => {
                    properties.extend(fields.clone());
                    let names = schema["required"].as_array().into_iter().flatten();
                    required.extend(names.filter_map(Value::as_str).map(str::to_owned));
                    Body::Fields
                }
                None => {
                    if request["required"] == true {
                        required.push("body".to_owned());
                    }
                    properties.insert("body".to_owned(), schema);
                    Body::Whole
                }
            }
        }
    };

    let mut input = json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        input["required"] = json!(required);
    }
    let summary = definition["summary"].as_str();
    let description = definition["description"]
        .as_str()
        .or(summary)
        .map_or_else(|| format!("{method} {path}"), str::to_owned);
    let read_only = matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    );
    let mut tool = json!({
        "name": name,
        "description": description,
        "inputSchema": input,
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": !read_only,
            "idempotentHint": read_only || matches!(*method, Method::PUT | Method::DELETE),
        },
    });
    if let Some(summary) = summary {
        tool["title"] = json!(summary);
    }
    Some(Operation {
        name,
        method: method.clone(),
        path: path.to_owned(),
        definition: definition.clone(),
        parameters,
        body,
        required,
        tool,
    })
}

/// The headers the transport sets itself, so no argument may set them: the
/// caller's token, the representation, and the headers that name the host or
/// frame the body, through which an argument could point the request at
/// another virtual host or corrupt it.
const TRANSPORT_HEADERS: [&str; 6] = [
    "accept",
    "content-type",
    "authorization",
    "host",
    "content-length",
    "transfer-encoding",
];

/// Where a parameter travels, or `None` for one that is not an argument: a
/// cookie, or one of the [`TRANSPORT_HEADERS`].
fn location(parameter: &Value) -> Option<Location> {
    let name = parameter["name"].as_str().unwrap_or_default();
    match parameter["in"].as_str()? {
        "path" => Some(Location::Path),
        "query" => Some(Location::Query),
        "header"
            if !TRANSPORT_HEADERS
                .iter()
                .any(|own| name.eq_ignore_ascii_case(own)) =>
        {
            Some(Location::Header)
        }
        _ => None,
    }
}

/// An `operationId` as a tool name, or `None` when it is empty.
fn tool_name(operation_id: &str) -> Option<String> {
    let name: String = operation_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect();
    (!name.is_empty()).then_some(name)
}

/// `node` with every local `$ref` replaced by its target.
///
/// Keywords beside a `$ref` apply alongside its target, as OpenAPI 3.1 and
/// JSON Schema 2020-12 define; [`beside`] joins them. A reference already
/// being inlined (a cycle), one that points nowhere, and every reference past
/// the budget stay as they are.
fn inline(node: &Value, document: &Value, seen: &mut Vec<String>, budget: &mut usize) -> Value {
    if *budget == 0 {
        return node.clone();
    }
    *budget -= 1;
    match node {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| inline(item, document, seen, budget))
                .collect(),
        ),
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                let target = reference
                    .strip_prefix('#')
                    .and_then(|pointer| document.pointer(pointer));
                return match target {
                    Some(target) if !seen.iter().any(|open| open == reference) => {
                        seen.push(reference.to_owned());
                        let resolved = inline(target, document, seen, budget);
                        seen.pop();
                        let keywords = object
                            .iter()
                            .filter(|(key, _)| key.as_str() != "$ref")
                            .map(|(key, value)| {
                                (key.clone(), inline(value, document, seen, budget))
                            })
                            .collect();
                        beside(resolved, keywords)
                    }
                    _ => node.clone(),
                };
            }
            Value::Object(
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), inline(value, document, seen, budget)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// Keywords that describe a schema without constraining it. Beside a `$ref`
/// they describe this use of the target, so they replace the target's own.
const ANNOTATIONS: [&str; 11] = [
    "title",
    "summary",
    "description",
    "default",
    "example",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
    "externalDocs",
    "$comment",
];

/// `target` with the `keywords` that sat beside its `$ref`, so that both hold.
///
/// A keyword the target lacks is added and an annotation replaces the
/// target's. `required` lists and `allOf` lists are joined, and `properties`
/// are merged, a property both declare differently becoming the `allOf` of
/// the two. Any other keyword the target sets differently is added under
/// `allOf`, so neither constraint is lost: `maximum: 10` beside a target with
/// `maximum: 100` keeps both. A target that is not an object, such as a
/// boolean schema, goes under `allOf` beside the keywords.
fn beside(target: Value, keywords: Map<String, Value>) -> Value {
    if keywords.is_empty() {
        return target;
    }
    let Value::Object(mut schema) = target else {
        let mut schema = keywords;
        all_of(&mut schema, vec![target]);
        return Value::Object(schema);
    };
    let mut apart = Vec::new();
    for (key, value) in keywords {
        let Some(own) = schema.get_mut(&key) else {
            schema.insert(key, value);
            continue;
        };
        match (own, value) {
            (own, value) if *own == value => {}
            (own, value) if ANNOTATIONS.contains(&key.as_str()) => *own = value,
            (Value::Array(own), Value::Array(more)) if key == "required" => {
                for name in more {
                    if !own.contains(&name) {
                        own.push(name);
                    }
                }
            }
            (Value::Array(own), Value::Array(more)) if key == "allOf" => own.extend(more),
            (Value::Object(own), Value::Object(more)) if key == "properties" => {
                for (name, property) in more {
                    match own.get_mut(&name) {
                        Some(existing) if *existing != property => {
                            *existing = json!({ "allOf": [existing.take(), property] });
                        }
                        Some(_) => {}
                        None => {
                            own.insert(name, property);
                        }
                    }
                }
            }
            (_, value) => {
                let mut keyword = Map::new();
                keyword.insert(key, value);
                apart.push(Value::Object(keyword));
            }
        }
    }
    all_of(&mut schema, apart);
    Value::Object(schema)
}

/// Appends `schemas` to the `allOf` of `schema`.
fn all_of(schema: &mut Map<String, Value>, schemas: Vec<Value>) {
    if schemas.is_empty() {
        return;
    }
    match schema.entry("allOf").or_insert_with(|| json!([])) {
        Value::Array(items) => items.extend(schemas),
        other => {
            let own = other.take();
            *other = Value::Array(std::iter::once(own).chain(schemas).collect());
        }
    }
}

/// One call: the arguments as a request, the API's answer as a value.
///
/// A `null` argument is a missing one, except for a field of a flattened JSON
/// body: there `null` is a value the API reads, as JSON Merge Patch (RFC 7396)
/// reads it as removing the field, so it is sent.
async fn forward(
    operation: Arc<Operation>,
    upstream: Arc<Upstream>,
    mut arguments: Map<String, Value>,
    authorization: Option<String>,
) -> Result<Value, Failure> {
    let is_parameter = |name: &String| operation.parameters.iter().any(|p| p.name == *name);
    arguments.retain(|name, value| {
        !value.is_null() || (operation.body == Body::Fields && !is_parameter(name))
    });
    let missing: Vec<&str> = operation
        .required
        .iter()
        .filter(|name| !arguments.contains_key(*name))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(invalid(format!(
            "Missing required arguments: {}",
            missing.join(", ")
        )));
    }
    let mut path = operation.path.clone();
    let mut query = Vec::new();
    let mut headers = Vec::new();
    for parameter in &operation.parameters {
        let Some(value) = arguments.remove(&parameter.name) else {
            continue;
        };
        let items: Vec<String> = match value {
            Value::Array(items) => items.iter().map(text).collect(),
            other => vec![text(&other)],
        };
        match parameter.location {
            Location::Path => {
                let value = items.join(",");
                path = path.replace(&format!("{{{}}}", parameter.name), &encode(&value));
            }
            Location::Query => query.extend(
                items
                    .iter()
                    .map(|item| format!("{}={}", encode(&parameter.name), encode(item))),
            ),
            Location::Header => {
                let name = HeaderName::try_from(parameter.name.as_str());
                let value = HeaderValue::try_from(items.join(","));
                let (Ok(name), Ok(value)) = (name, value) else {
                    return Err(invalid(format!(
                        "{} cannot be sent as a header",
                        parameter.name
                    )));
                };
                headers.push((name, value));
            }
        }
    }
    if path.split('/').any(|segment| matches!(segment, "." | "..")) {
        return Err(invalid(
            "Path arguments cannot form a dot path segment".to_owned(),
        ));
    }
    let mut url = format!("{}{path}", upstream.base_url.trim_end_matches('/'));
    if !query.is_empty() {
        url = format!("{url}?{}", query.join("&"));
    }
    let mut request = upstream
        .http
        .request(operation.method.clone(), url)
        .header(header::ACCEPT, "application/json");
    if let Some(authorization) = authorization.filter(|_| upstream.forward_caller_token) {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    for (name, value) in &upstream.headers {
        request = request.header(name, value);
    }
    let configured = |name: &HeaderName| upstream.headers.iter().any(|(fixed, _)| fixed == name);
    for (name, value) in headers.into_iter().filter(|(name, _)| !configured(name)) {
        request = request.header(name, value);
    }
    request = match operation.body {
        Body::None => request,
        Body::Whole => match arguments.remove("body") {
            Some(body) => request.json(&body),
            None => request,
        },
        Body::Fields => request.json(&arguments),
    };
    let response = request.send().await.map_err(no_answer)?;
    let status = response.status();
    let body = crate::client::read_bounded(response, upstream.limit)
        .await
        .map_err(|error| match error {
            RuntimeError::LimitExceeded { .. } => {
                Failure::new(StatusCode::BAD_GATEWAY, codes::LIMIT_EXCEEDED, "")
            }
            other => Failure::new(StatusCode::BAD_GATEWAY, UPSTREAM_UNAVAILABLE, "")
                .with_detail(crate::error_chain(&other)),
        })?;
    if status.is_success() {
        return Ok(if body.is_empty() {
            Value::String(format!("HTTP {}", status.as_u16()))
        } else {
            serde_json::from_slice(&body)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
        });
    }
    if status.is_client_error() {
        let message = format!(
            "The API refused the call with HTTP {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(&body)
        );
        return Err(Failure::new(status, UPSTREAM_REFUSED, message));
    }
    Err(
        Failure::new(StatusCode::BAD_GATEWAY, UPSTREAM_UNAVAILABLE, "")
            .with_detail(format!("the API answered HTTP {}", status.as_u16())),
    )
}

/// A `400` for arguments that cannot make a request.
fn invalid(message: String) -> Failure {
    Failure::new(StatusCode::BAD_REQUEST, INVALID_ARGUMENTS, message)
}

/// A request that got no answer: a `504` when the client's time limit ran
/// out, else a `502`. The URL never reaches the detail.
fn no_answer(error: reqwest::Error) -> Failure {
    if error.is_timeout() {
        return Failure::new(StatusCode::GATEWAY_TIMEOUT, codes::TIMEOUT, "");
    }
    let detail = crate::error_chain(&crate::client::send_error("calling the API", error));
    Failure::new(StatusCode::BAD_GATEWAY, UPSTREAM_UNAVAILABLE, "").with_detail(detail)
}

/// A scalar argument as text: a string as it is, anything else as JSON.
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Percent-encodes every byte outside the RFC 3986 unreserved set.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}
