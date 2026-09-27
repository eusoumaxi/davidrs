//! The MCP server: the protocol on the wire, its routes, its policy and
//! origin checks, and hand-written tools.
#![cfg(feature = "mcp")]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use davidrs::http::access::{Access, Claims, Grant};
use davidrs::http::{Admission, Failure, HeaderMap, HttpResponse, Public, Request, StatusCode};
use davidrs::mcp::{ProtectedResource, Server, Tool, INVALID_ARGUMENTS, MAX_TOOLS};
use davidrs::{Context, Invocation};
use lambda_http::{Body, RequestExt as _};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{json, Value};

const VERSION: &str = "2026-07-28";

/// The application state the tools read.
struct App {
    greeting: String,
}

fn app() -> Arc<App> {
    Arc::new(App {
        greeting: "Hello".to_owned(),
    })
}

#[derive(Deserialize)]
struct Add {
    a: i64,
    b: i64,
}

#[derive(Serialize)]
struct Sum {
    sum: i64,
}

async fn add(_: Arc<App>, input: Add, _: Context<()>) -> Result<Sum, Failure> {
    Ok(Sum {
        sum: input.a + input.b,
    })
}

#[derive(Deserialize)]
struct Name {
    name: String,
}

async fn greet(app: Arc<App>, input: Name, _: Context<()>) -> Result<String, Failure> {
    Ok(format!("{}, {}", app.greeting, input.name))
}

async fn digits(_: Arc<App>, _: Value, _: Context<()>) -> Result<Vec<u8>, Failure> {
    Ok(vec![1, 2, 3])
}

async fn refuse(_: Arc<App>, _: Value, _: Context<()>) -> Result<Value, Failure> {
    Err(Failure::new(
        StatusCode::CONFLICT,
        "ERROR_ORDER_CLOSED",
        "The order is closed",
    ))
}

async fn crash(_: Arc<App>, _: Value, _: Context<()>) -> Result<Value, Failure> {
    Err(Failure::internal("FAULT_STORE", "connect 10.0.0.7 refused"))
}

async fn sleep(_: Arc<App>, _: Value, _: Context<()>) -> Result<Value, Failure> {
    tokio::time::sleep(Duration::from_secs(5)).await;
    Ok(Value::Null)
}

/// A success value whose serialization fails.
struct Unwritable;

impl Serialize for Unwritable {
    fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("cannot be written"))
    }
}

async fn unwritable(_: Arc<App>, _: Value, _: Context<()>) -> Result<Unwritable, Failure> {
    Ok(Unwritable)
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

/// A public server with one tool per behaviour under test.
fn server() -> Server<App, Public> {
    Server::new("orders", "1.2.3", Public)
        .instructions("Use add for sums.")
        .tool(Tool::new(
            "add",
            "Adds two integers",
            object(json!({ "a": {}, "b": {} }), &["a", "b"]),
            add,
        ))
        .tool(
            Tool::new(
                "greet",
                "Greets someone",
                object(json!({ "name": {} }), &["name"]),
                greet,
            )
            .annotations(json!({ "readOnlyHint": true })),
        )
        .tool(Tool::new("digits", "Three digits", json!({}), digits))
        .tool(Tool::new("refuse", "Always refuses", json!({}), refuse))
        .tool(Tool::new("crash", "Always fails", json!({}), crash))
        .tool(Tool::new("sleep", "Takes five seconds", json!({}), sleep))
        .tool(Tool::new(
            "unwritable",
            "Cannot answer",
            json!({}),
            unwritable,
        ))
}

/// An HTTP API (v2) event with these headers, body and authorizer.
fn event(
    method: &str,
    path: &str,
    headers: Value,
    body: Option<&Value>,
    authorizer: Option<Value>,
) -> lambda_http::Request {
    let mut context = json!({
        "http": { "method": method, "path": path, "sourceIp": "203.0.113.9" },
        "requestId": "r1",
        "stage": "$default",
        "timeEpoch": 0
    });
    if let Some(authorizer) = authorizer {
        context["authorizer"] = authorizer;
    }
    let mut event = json!({
        "version": "2.0",
        "rawPath": path,
        "rawQueryString": "",
        "headers": headers,
        "requestContext": context,
        "isBase64Encoded": false
    });
    if let Some(body) = body {
        event["body"] = json!(body.to_string());
    }
    lambda_http::request::from_str(&event.to_string()).expect("an HTTP API event")
}

fn post(headers: Value, body: &Value) -> lambda_http::Request {
    event("POST", "/", headers, Some(body), None)
}

/// The headers a client mirrors from the body.
fn mirrored(method: &str, name: Option<&str>) -> Value {
    let mut headers = json!({ "mcp-protocol-version": VERSION, "mcp-method": method });
    if let Some(name) = name {
        headers["mcp-name"] = json!(name);
    }
    headers
}

/// A request body: `params` plus the metadata every request carries.
fn body_of(method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": VERSION,
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
}

/// A well-formed request.
fn request(method: &str, params: Value) -> lambda_http::Request {
    let name = params["name"].as_str().map(str::to_owned);
    post(mirrored(method, name.as_deref()), &body_of(method, params))
}

fn call(name: &str, arguments: Value) -> lambda_http::Request {
    request(
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
}

/// Attaches a Lambda context whose deadline is `budget_ms` from now.
fn with_budget(request: lambda_http::Request, budget_ms: u64) -> lambda_http::Request {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let mut context = lambda_runtime::Context::default();
    context.deadline = now + budget_ms;
    request.with_lambda_context(context)
}

fn json_body(response: &HttpResponse) -> Value {
    match response.body() {
        Body::Text(text) => serde_json::from_str(text).expect("a JSON body"),
        other => panic!("expected a text body, got {other:?}"),
    }
}

async fn send<P: davidrs::http::Policy, A: Admission>(
    server: &Server<App, P, A>,
    request: lambda_http::Request,
) -> (StatusCode, HeaderMap, Value) {
    let response = server.handle(app(), request).await;
    let body = match response.body() {
        Body::Empty => Value::Null,
        _ => json_body(&response),
    };
    (response.status(), response.headers().clone(), body)
}

/// The text of a tool result, and whether it is an error.
fn tool_text(body: &Value) -> (&str, bool) {
    let result = &body["result"];
    (
        result["content"][0]["text"].as_str().expect("text content"),
        result["isError"].as_bool().expect("isError"),
    )
}

#[tokio::test]
async fn a_discovery_names_every_revision_the_server_and_its_instructions() {
    let (status, _, body) = send(&server(), request("server/discover", json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], 1);
    let result = &body["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["supportedVersions"], json!([VERSION]));
    assert_eq!(result["capabilities"], json!({ "tools": {} }));
    assert_eq!(result["instructions"], "Use add for sums.");
    assert_eq!(result["cacheScope"], "public");
    assert!(result["ttlMs"].as_u64().is_some());
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"],
        json!({ "name": "orders", "version": "1.2.3" })
    );
}

/// An `initialize` from a client on an earlier revision carries no protocol
/// metadata: it is refused with the revision this server speaks, which is
/// what such a client can show its user.
#[tokio::test]
async fn a_request_for_another_revision_is_refused_with_the_supported_one() {
    let initialize = post(
        json!({}),
        &json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }),
    );
    let header_only = post(
        json!({ "mcp-protocol-version": "2025-11-25", "mcp-method": "tools/list" }),
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    for (request, requested) in [
        (initialize, Value::Null),
        (header_only, json!("2025-11-25")),
    ] {
        let (status, headers, body) = send(&server(), request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(headers.get("mcp-session-id").is_none());
        assert_eq!(body["error"]["code"], -32022);
        assert_eq!(
            body["error"]["data"],
            json!({ "supported": [VERSION], "requested": requested })
        );
    }
}

#[tokio::test]
async fn an_unsupported_revision_in_the_metadata_is_refused() {
    let mut body = body_of("ping", json!({}));
    body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2027-01-01");
    let headers = json!({ "mcp-protocol-version": "2027-01-01", "mcp-method": "ping" });
    let (status, _, answer) = send(&server(), post(headers, &body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(answer["error"]["code"], -32022);
    assert_eq!(answer["error"]["data"]["requested"], "2027-01-01");
}

#[tokio::test]
async fn a_server_without_instructions_describes_none() {
    let server = Server::<App, _>::new("bare", "0.1.0", Public);
    let (_, _, body) = send(&server, request("server/discover", json!({}))).await;
    assert!(body["result"].get("instructions").is_none());
}

#[tokio::test]
async fn a_notification_is_accepted_with_no_body() {
    let request = post(
        json!({}),
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );
    let (status, headers, body) = send(&server(), request).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(headers.get("content-type").is_none());
    assert_eq!(body, Value::Null);
}

#[tokio::test]
async fn a_ping_is_an_empty_complete_result() {
    let (_, _, body) = send(&server(), request("ping", json!({}))).await;
    assert_eq!(body["result"]["resultType"], "complete");
    assert_eq!(
        body["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "orders"
    );
}

#[tokio::test]
async fn an_unknown_method_is_a_404() {
    for method in ["resources/list", "initialize"] {
        let (status, _, body) = send(&server(), request(method, json!({}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], -32601);
    }
}

#[tokio::test]
async fn a_body_that_is_not_json_is_a_parse_error() {
    let request = event("POST", "/", json!({}), None, None);
    let (status, _, body) = send(&server(), request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], -32700);
    assert!(body.get("id").is_none());
}

#[tokio::test]
async fn a_body_that_is_not_one_request_is_an_invalid_request() {
    let cases = [
        (
            json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }]),
            None,
        ),
        (
            json!({ "jsonrpc": "2.0", "id": {}, "method": "ping" }),
            None,
        ),
        (
            json!({ "jsonrpc": "1.0", "id": 3, "method": "ping" }),
            Some(3),
        ),
        (json!({ "jsonrpc": "2.0", "id": "a", "method": "" }), None),
        (json!({ "jsonrpc": "2.0", "id": 4 }), Some(4)),
    ];
    for (message, id) in cases {
        let (status, _, body) = send(&server(), post(json!({}), &message)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{message}");
        assert_eq!(body["error"]["code"], -32600, "{message}");
        if let Some(id) = id {
            assert_eq!(body["id"], id);
        }
    }
}

/// The header names the revision, but the metadata the body must carry is
/// incomplete.
#[tokio::test]
async fn metadata_without_the_revision_or_the_capabilities_is_invalid_params() {
    let bodies = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "ping",
            "params": { "_meta": { "io.modelcontextprotocol/protocolVersion": VERSION } }
        }),
    ];
    for body in bodies {
        let (status, _, answer) = send(&server(), post(mirrored("ping", None), &body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(answer["error"]["code"], -32602, "{body}");
    }
}

#[tokio::test]
async fn a_missing_or_mismatched_mirrored_header_is_a_header_mismatch() {
    let body = body_of("tools/list", json!({}));
    let cases = [
        json!({ "mcp-method": "tools/list" }),
        json!({ "mcp-protocol-version": "2025-11-25", "mcp-method": "tools/list" }),
        json!({ "mcp-protocol-version": VERSION }),
        json!({ "mcp-protocol-version": VERSION, "mcp-method": "ping" }),
    ];
    for headers in cases {
        let (status, _, answer) = send(&server(), post(headers.clone(), &body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{headers}");
        assert_eq!(answer["error"]["code"], -32020, "{headers}");
    }
}

#[tokio::test]
async fn a_tool_call_must_mirror_the_tool_name() {
    let body = body_of("tools/call", json!({ "name": "add", "arguments": {} }));
    for name in [None, Some("greet"), Some("=?base64?not base64?=")] {
        let (_, _, answer) = send(&server(), post(mirrored("tools/call", name), &body)).await;
        assert_eq!(answer["error"]["code"], -32020, "{name:?}");
    }
}

#[tokio::test]
async fn a_base64_tool_name_header_is_decoded_before_it_is_compared() {
    let body = body_of(
        "tools/call",
        json!({ "name": "greet", "arguments": { "name": "Ada" } }),
    );
    let encoded = format!(
        "=?base64?{}?=",
        base64::engine::general_purpose::STANDARD.encode("greet")
    );
    let (status, _, answer) = send(
        &server(),
        post(mirrored("tools/call", Some(&encoded)), &body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tool_text(&answer), ("Hello, Ada", false));
}

#[tokio::test]
async fn the_tool_list_holds_every_definition_in_the_order_they_were_added() {
    let (_, _, body) = send(&server(), request("tools/list", json!({}))).await;
    let tools = body["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        names,
        [
            "add",
            "greet",
            "digits",
            "refuse",
            "crash",
            "sleep",
            "unwritable"
        ]
    );
    assert_eq!(tools[0]["description"], "Adds two integers");
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["a", "b"]));
    assert_eq!(tools[1]["annotations"]["readOnlyHint"], true);
    assert_eq!(body["result"]["cacheScope"], "public");
}

#[tokio::test]
async fn an_object_success_is_structured_content_and_text() {
    let (_, _, body) = send(&server(), call("add", json!({ "a": 2, "b": 3 }))).await;
    let result = &body["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["structuredContent"], json!({ "sum": 5 }));
    assert_eq!(tool_text(&body), ("{\"sum\":5}", false));
}

#[tokio::test]
async fn a_success_that_is_not_an_object_is_text_only() {
    let (_, _, greeting) = send(&server(), call("greet", json!({ "name": "Ada" }))).await;
    assert_eq!(tool_text(&greeting), ("Hello, Ada", false));
    let (_, _, list) = send(&server(), call("digits", Value::Null)).await;
    assert_eq!(tool_text(&list), ("[1,2,3]", false));
    assert!(list["result"].get("structuredContent").is_none());
}

#[tokio::test]
async fn arguments_that_do_not_fit_the_tool_are_a_tool_error() {
    let (status, _, body) = send(&server(), call("add", json!({ "a": "two" }))).await;
    assert_eq!(status, StatusCode::OK);
    let (text, error) = tool_text(&body);
    assert!(error);
    assert!(text.starts_with(INVALID_ARGUMENTS), "{text}");
}

#[tokio::test]
async fn a_refusal_reaches_the_model_and_a_server_failure_is_redacted() {
    let (_, _, refused) = send(&server(), call("refuse", json!({}))).await;
    assert_eq!(
        tool_text(&refused),
        ("ERROR_ORDER_CLOSED: The order is closed", true)
    );
    let (_, _, crashed) = send(&server(), call("crash", json!({}))).await;
    let (text, error) = tool_text(&crashed);
    assert!(error);
    assert_eq!(text, "FAULT_STORE: InternalServerError");
}

#[tokio::test]
async fn a_success_that_cannot_be_serialized_is_a_tool_error() {
    let (_, _, body) = send(&server(), call("unwritable", json!({}))).await;
    assert_eq!(
        tool_text(&body),
        ("FAULT_SERIALIZATION: InternalServerError", true)
    );
}

/// The invocation has 1.5 s left; the tool gets that less the one second
/// margin, and answers with a result long before Lambda would stop it.
#[tokio::test]
async fn a_tool_that_outlives_its_deadline_is_a_timeout_result() {
    let started = std::time::Instant::now();
    let request = with_budget(call("sleep", json!({})), 1_500);
    let (status, _, body) = send(&server(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        tool_text(&body),
        ("ERROR_TIMEOUT: InternalServerError", true)
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn an_unknown_tool_is_a_protocol_error() {
    let (status, _, body) = send(&server(), call("missing", json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], -32602);
    assert_eq!(body["error"]["message"], "Unknown tool: missing");
}

#[tokio::test]
async fn arguments_that_are_not_an_object_are_malformed() {
    let (status, _, body) = send(&server(), call("add", json!([1, 2]))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], -32602);
    assert_eq!(body["id"], 1);
}

#[tokio::test]
async fn a_hidden_tool_is_neither_listed_nor_callable() {
    let server = server().visible(|tool, _: &()| tool != "crash");
    let (_, _, list) = send(&server, request("tools/list", json!({}))).await;
    let tools = list["result"]["tools"].as_array().expect("tools");
    assert!(tools.iter().all(|tool| tool["name"] != "crash"));
    assert_eq!(tools.len(), 6);
    assert_eq!(list["result"]["cacheScope"], "private");
    let (_, _, called) = send(&server, call("crash", json!({}))).await;
    assert_eq!(called["error"]["code"], -32602);
}

#[tokio::test]
async fn a_foreign_browser_origin_is_refused_before_anything_is_parsed() {
    let server = server().allow_origins(["https://app.example.com"]);
    let foreign = post(
        json!({ "origin": "https://evil.example" }),
        &json!("not a message"),
    );
    let (status, _, body) = send(&server, foreign).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], -32600);
    assert!(body.get("id").is_none());

    let mut headers = mirrored("ping", None);
    headers["origin"] = json!("https://app.example.com");
    let (status, _, _) = send(&server, post(headers, &body_of("ping", json!({})))).await;
    assert_eq!(status, StatusCode::OK);
}

/// Refuses requests that carry `x-quota: spent`, the way a rate limiter
/// refuses a caller whose budget is gone.
struct Quota;

impl Admission for Quota {
    async fn check(
        &self,
        request: &Request<'_>,
        _: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        let budget = vec![("ratelimit".to_owned(), "\"default\";r=0;t=30".to_owned())];
        if request.header("x-quota") == Some("spent") {
            return Err(Failure::new(
                StatusCode::TOO_MANY_REQUESTS,
                "ERROR_RATE_LIMITED",
                "Slow down",
            )
            .with_headers(budget));
        }
        Ok(budget)
    }
}

#[tokio::test]
async fn the_admission_counts_every_request_and_its_refusal_keeps_its_headers() {
    let server = server().admission(Quota);
    let mut headers = mirrored("ping", None);
    headers["x-quota"] = json!("spent");
    let (status, answered, body) = send(&server, post(headers, &body_of("ping", json!({})))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(answered["ratelimit"], "\"default\";r=0;t=30");
    assert_eq!(body["error"]["message"], "Slow down");

    let (status, answered, _) = send(&server, request("ping", json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(answered.contains_key("ratelimit"));
}

#[tokio::test]
async fn any_other_method_than_post_is_a_405_naming_post() {
    for method in ["GET", "DELETE", "PUT"] {
        let request = event(method, "/", json!({}), None, None);
        let (status, headers, body) = send(&server(), request).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(headers["allow"], "POST");
        assert_eq!(body["error"]["code"], -32600);
    }
}

#[tokio::test]
async fn a_body_over_the_limit_is_refused() {
    let server = server().body_limit(64);
    let request = call("greet", json!({ "name": "x".repeat(100) }));
    let (status, _, body) = send(&server, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"]["code"], -32600);
}

#[tokio::test]
async fn an_invocation_out_of_time_is_a_504_internal_error() {
    let request = with_budget(request("ping", json!({})), 50);
    let (status, _, body) = send(&server(), request).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body["error"]["code"], -32603);
    assert_eq!(body["error"]["message"], "InternalServerError");
}

/// The application's caller in the authorization tests.
#[derive(Debug)]
struct User {
    id: String,
}

fn user(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.subject()?.to_owned(),
    })
}

async fn whoami(
    _: Arc<App>,
    _: Value,
    context: Context<Grant<User, ()>>,
) -> Result<String, Failure> {
    Ok(context.scope().caller().id.clone())
}

fn protected(resource: Option<ProtectedResource>) -> Server<App, Access<User, User>> {
    let server = Server::new("orders", "1.2.3", Access::new(user).require_caller())
        .tool(Tool::new("whoami", "Names the caller", json!({}), whoami));
    match resource {
        Some(resource) => server.protected_resource(resource),
        None => server,
    }
}

#[tokio::test]
async fn a_caller_the_policy_refuses_is_pointed_at_the_resource_metadata() {
    let resource = ProtectedResource::new("https://example.com/mcp/", ["https://id.example.com"])
        .scopes(["orders:read", "orders:write"]);
    let (status, headers, body) =
        send(&protected(Some(resource)), request("ping", json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers["www-authenticate"],
        "Bearer resource_metadata=\"https://example.com/.well-known/oauth-protected-resource/mcp\", scope=\"orders:read orders:write\""
    );
    assert_eq!(body["error"]["code"], -32600);
    assert_eq!(body["error"]["message"], "Authentication is required");
}

#[tokio::test]
async fn a_refusal_without_resource_metadata_still_names_the_bearer_scheme() {
    let (status, headers, _) = send(&protected(None), request("ping", json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["www-authenticate"], "Bearer");
}

#[tokio::test]
async fn a_tool_receives_the_scope_the_policy_established() {
    let body = body_of("tools/call", json!({ "name": "whoami" }));
    let request = event(
        "POST",
        "/",
        mirrored("tools/call", Some("whoami")),
        Some(&body),
        Some(json!({ "jwt": { "claims": { "sub": "alice" }, "scopes": null } })),
    );
    let (_, _, answer) = send(&protected(None), request).await;
    assert_eq!(tool_text(&answer), ("alice", false));
}

#[tokio::test]
async fn the_resource_metadata_is_public_at_either_well_known_path() {
    let resource = ProtectedResource::new("https://mcp.example.com", ["https://id.example.com"])
        .scopes(["orders:read"]);
    let server = protected(Some(resource));
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let (status, _, body) = send(&server, event("GET", path, json!({}), None, None)).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(
            body,
            json!({
                "resource": "https://mcp.example.com",
                "authorization_servers": ["https://id.example.com"],
                "bearer_methods_supported": ["header"],
                "scopes_supported": ["orders:read"]
            })
        );
    }
}

#[tokio::test]
async fn resource_metadata_without_scopes_lists_none_and_challenges_without_them() {
    let resource = ProtectedResource::new("https://mcp.example.com", ["https://id.example.com"]);
    let server = protected(Some(resource));
    let metadata = event(
        "GET",
        "/.well-known/oauth-protected-resource",
        json!({}),
        None,
        None,
    );
    let (_, _, body) = send(&server, metadata).await;
    assert!(body.get("scopes_supported").is_none());
    let (_, headers, _) = send(&server, request("ping", json!({}))).await;
    assert_eq!(
        headers["www-authenticate"],
        "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource\""
    );
}

#[tokio::test]
async fn a_server_without_resource_metadata_answers_its_path_with_404() {
    let path = "/.well-known/oauth-protected-resource";
    let (status, _, _) = send(&server(), event("GET", path, json!({}), None, None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let lookalike = event(
        "GET",
        "/.well-known/oauth-protected-resources",
        json!({}),
        None,
        None,
    );
    let (status, _, _) = send(&server(), lookalike).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[test]
fn every_part_of_a_server_describes_itself_for_debugging() {
    let server = server().protected_resource(ProtectedResource::new(
        "https://mcp.example.com",
        ["https://id.example.com"],
    ));
    let text = format!("{server:?}");
    assert!(text.contains("orders") && text.contains("greet") && text.contains("mcp.example.com"));
}

#[test]
#[should_panic(expected = "duplicate tool name")]
fn two_tools_with_one_name_are_a_startup_panic() {
    let _ = server().tool(Tool::new("add", "Again", json!({}), digits));
}

#[test]
#[should_panic(expected = "invalid tool name")]
fn a_tool_name_with_a_space_is_a_startup_panic() {
    let _ = server().tool(Tool::new("add two", "Spaced", json!({}), digits));
}

#[test]
#[should_panic(expected = "more than 128 tools")]
fn more_tools_than_the_maximum_is_a_startup_panic() {
    let mut server = Server::<App, _>::new("many", "1.0.0", Public);
    for index in 0..=MAX_TOOLS {
        server = server.tool(Tool::new(
            format!("tool{index}"),
            "One of many",
            json!({}),
            digits,
        ));
    }
}
