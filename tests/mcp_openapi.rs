//! Tools from an OpenAPI document: how operations become tools, and how a
//! call becomes one request to the API.
#![cfg(feature = "mcp-openapi")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use davidrs::RuntimeError;
use davidrs::client::{self, Limits};
use davidrs::http::{HeaderName, HeaderValue};
use davidrs::http::{HttpResponse, Public};
use davidrs::mcp::openapi::{OpenApi, UPSTREAM_REFUSED, UPSTREAM_UNAVAILABLE};
use davidrs::mcp::{PROTOCOL_VERSION, Server};
use lambda_http::Body;
use serde_json::{Value, json};
use support::server::{self, Recorded};

/// Paths are written in sorted order, so the tools come out in the same
/// order whether or not the JSON map keeps the document's order.
fn document() -> Value {
    json!({
        "openapi": "3.1.0",
        "paths": {
            "/files": {
                "post": {
                    "operationId": "uploadFile",
                    "requestBody": { "content": { "multipart/form-data": { "schema": {} } } }
                }
            },
            "/health": { "get": { "summary": "Has no operationId" } },
            "/orders": {
                "parameters": [{ "$ref": "#/components/parameters/Tenant" }],
                "get": {
                    "operationId": "listOrders",
                    "summary": "List orders",
                    "tags": ["orders"],
                    "parameters": [
                        { "name": "status", "in": "query", "description": "Only this status", "schema": { "type": "string" } },
                        { "name": "tag", "in": "query", "schema": { "type": "array", "items": { "type": "string" } } },
                        { "name": "Accept", "in": "header" },
                        { "name": "Host", "in": "header" },
                        { "name": "Transfer-Encoding", "in": "header" },
                        { "name": "session", "in": "cookie" },
                        { "name": "X-Tenant", "in": "header", "required": true, "schema": { "type": "string" } }
                    ]
                },
                "post": {
                    "operationId": "create order!",
                    "description": "Creates an order.",
                    "x-internal": true,
                    "requestBody": { "$ref": "#/components/requestBodies/NewOrder" }
                }
            },
            "/orders/{id}": {
                "put": {
                    "operationId": "replaceOrder",
                    "parameters": [{ "name": "id", "in": "path" }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": { "id": { "type": "string" }, "note": { "type": "string" } }
                        } } }
                    }
                },
                "delete": {
                    "operationId": "deleteOrder",
                    "parameters": [{ "name": "id", "in": "path" }]
                },
                "patch": {
                    "operationId": "tagOrder",
                    "parameters": [{ "name": "id", "in": "path" }],
                    "requestBody": { "content": { "application/json": { "schema": { "type": "array", "items": { "type": "string" } } } } }
                }
            },
            "/tree": {
                "get": {
                    "operationId": "getTree",
                    "parameters": [{ "name": "node", "in": "query", "schema": { "$ref": "#/components/schemas/Node" } }]
                }
            }
        },
        "components": {
            "parameters": {
                "Tenant": { "name": "X-Tenant", "in": "header", "schema": { "type": "string" } }
            },
            "requestBodies": {
                "NewOrder": {
                    "required": true,
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/NewOrder" } } }
                }
            },
            "schemas": {
                "NewOrder": {
                    "type": "object",
                    "required": ["item"],
                    "properties": {
                        "item": { "type": "string" },
                        "quantity": { "type": "integer" },
                        "link": { "$ref": "#/components/schemas/Missing" }
                    }
                },
                "Node": { "type": "object", "properties": { "child": { "$ref": "#/components/schemas/Node" } } }
            }
        }
    })
}

fn http() -> reqwest::Client {
    client::build(Limits::default()).expect("client")
}

fn api(base_url: &str) -> OpenApi {
    OpenApi::new(&document(), base_url, http()).expect("an OpenAPI 3 document")
}

fn mcp(api: OpenApi) -> Server<(), Public> {
    Server::new("orders", "1.0.0", Public).openapi(api)
}

/// A request with its protocol metadata, its mirrored headers and `extra`
/// headers.
fn request(extra: Value, method: &str, mut params: Value) -> lambda_http::Request {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let mut headers = json!({ "mcp-protocol-version": PROTOCOL_VERSION, "mcp-method": method });
    if let Some(name) = params["name"].as_str() {
        headers["mcp-name"] = json!(name);
    }
    for (name, value) in extra.as_object().into_iter().flatten() {
        headers[name] = value.clone();
    }
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let event = json!({
        "version": "2.0",
        "rawPath": "/",
        "rawQueryString": "",
        "headers": headers,
        "requestContext": {
            "http": { "method": "POST", "path": "/", "sourceIp": "203.0.113.9" },
            "requestId": "r1",
            "stage": "$default",
            "timeEpoch": 0
        },
        "body": body.to_string(),
        "isBase64Encoded": false
    });
    lambda_http::request::from_str(&event.to_string()).expect("an HTTP API event")
}

fn json_body(response: &HttpResponse) -> Value {
    match response.body() {
        Body::Text(text) => serde_json::from_str(text).expect("a JSON body"),
        other => panic!("expected a text body, got {other:?}"),
    }
}

async fn tools(server: &Server<(), Public>) -> Vec<Value> {
    let response = server
        .handle(Arc::new(()), request(json!({}), "tools/list", json!({})))
        .await;
    json_body(&response)["result"]["tools"]
        .as_array()
        .expect("tools")
        .clone()
}

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

/// Calls `name` as a caller presenting `Bearer caller-token`.
async fn call(server: &Server<(), Public>, name: &str, arguments: Value) -> Value {
    let headers = json!({ "authorization": "Bearer caller-token" });
    let params = json!({ "name": name, "arguments": arguments });
    let response = server
        .handle(Arc::new(()), request(headers, "tools/call", params))
        .await;
    json_body(&response)["result"].clone()
}

fn text(result: &Value) -> (&str, bool) {
    (
        result["content"][0]["text"].as_str().expect("text"),
        result["isError"].as_bool().expect("isError"),
    )
}

/// An upstream that answers every request with this status and body.
async fn upstream(status: u16, content_type: &'static str, body: &'static str) -> server::Server {
    server::Server::fixed(status, content_type, body).await
}

fn only(upstream: &server::Server) -> Recorded {
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    requests[0].clone()
}

#[test]
fn a_document_that_is_not_openapi_3_is_a_configuration_error() {
    for document in [json!({ "swagger": "2.0", "paths": {} }), json!([])] {
        let error = OpenApi::new(&document, "https://api.example.com", http()).unwrap_err();
        assert!(matches!(error, RuntimeError::Configuration(_)), "{error}");
    }
}

/// Appended to the base URL, such a path would name another host and send
/// the caller's token there.
#[test]
fn a_path_that_does_not_start_with_a_slash_is_a_configuration_error() {
    let document = json!({
        "openapi": "3.1.0",
        "paths": { "@other.example/orders": { "get": { "operationId": "listOrders" } } }
    });
    let error = OpenApi::new(&document, "https://api.example.com", http()).unwrap_err();
    assert!(
        matches!(&error, RuntimeError::Configuration(message) if message.contains("@other.example/orders")),
        "{error}"
    );
}

#[test]
fn every_operation_with_an_id_and_a_json_body_is_read() {
    let api = api("https://api.example.com");
    let names: Vec<&str> = api
        .operations()
        .iter()
        .map(|operation| operation.name())
        .collect();
    assert_eq!(
        names,
        [
            "listOrders",
            "create_order_",
            "replaceOrder",
            "deleteOrder",
            "tagOrder",
            "getTree"
        ]
    );
    let create = &api.operations()[1];
    assert_eq!((create.method(), create.path()), ("POST", "/orders"));
}

#[test]
fn a_predicate_selects_by_method_tag_or_extension() {
    let tagged = api("https://api.example.com").select(|operation| operation.has_tag("orders"));
    assert_eq!(tagged.operations().len(), 1);
    let internal = api("https://api.example.com")
        .select(|operation| operation.definition()["x-internal"] == true);
    assert_eq!(internal.operations()[0].name(), "create_order_");
    let deletes = api("https://api.example.com").select(|operation| operation.method() == "DELETE");
    assert_eq!(deletes.operations()[0].name(), "deleteOrder");
}

#[tokio::test]
async fn parameters_become_arguments_and_the_transports_own_do_not() {
    let tools = tools(&mcp(api("https://api.example.com"))).await;
    let list = tool(&tools, "listOrders");
    assert_eq!(list["title"], "List orders");
    assert_eq!(list["description"], "List orders");
    let properties = list["inputSchema"]["properties"]
        .as_object()
        .expect("properties");
    let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["X-Tenant", "status", "tag"]);
    assert_eq!(properties["status"]["description"], "Only this status");
    assert_eq!(list["inputSchema"]["required"], json!(["X-Tenant"]));
    assert_eq!(
        list["annotations"],
        json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true })
    );
}

#[tokio::test]
async fn a_json_object_body_is_flattened_with_its_references_inlined() {
    let tools = tools(&mcp(api("https://api.example.com"))).await;
    let create = tool(&tools, "create_order_");
    assert_eq!(create["description"], "Creates an order.");
    assert!(create.get("title").is_none());
    let schema = &create["inputSchema"];
    assert_eq!(schema["properties"]["item"], json!({ "type": "string" }));
    assert_eq!(
        schema["properties"]["link"],
        json!({ "$ref": "#/components/schemas/Missing" })
    );
    assert_eq!(schema["required"], json!(["item"]));
    assert_eq!(create["annotations"]["readOnlyHint"], false);
    assert_eq!(create["annotations"]["idempotentHint"], false);
}

#[tokio::test]
async fn a_body_that_clashes_or_is_not_an_object_is_one_body_argument() {
    let tools = tools(&mcp(api("https://api.example.com"))).await;
    let replace = tool(&tools, "replaceOrder");
    assert_eq!(replace["description"], "PUT /orders/{id}");
    assert_eq!(replace["inputSchema"]["required"], json!(["id", "body"]));
    assert_eq!(
        replace["inputSchema"]["properties"]["id"],
        json!({ "type": "string" })
    );
    assert_eq!(
        replace["inputSchema"]["properties"]["body"]["type"],
        "object"
    );
    let tag = tool(&tools, "tagOrder");
    assert_eq!(tag["inputSchema"]["required"], json!(["id"]));
    assert_eq!(tag["inputSchema"]["properties"]["body"]["type"], "array");
    assert_eq!(
        tool(&tools, "deleteOrder")["annotations"]["destructiveHint"],
        true
    );
}

#[tokio::test]
async fn a_recursive_schema_keeps_its_reference_where_it_repeats() {
    let tools = tools(&mcp(api("https://api.example.com"))).await;
    let node = &tool(&tools, "getTree")["inputSchema"]["properties"]["node"];
    assert_eq!(node["type"], "object");
    assert_eq!(
        node["properties"]["child"],
        json!({ "$ref": "#/components/schemas/Node" })
    );
}

/// One operation may inline only so much: past the budget, references are
/// left for the client to resolve.
#[tokio::test]
async fn inlining_stops_at_its_budget() {
    let values: Vec<u32> = (0..10_001).collect();
    let document = json!({
        "openapi": "3.0.3",
        "paths": { "/big": { "get": {
            "operationId": "big",
            "parameters": [
                { "name": "size", "in": "query", "schema": { "enum": values } },
                { "name": "node", "in": "query", "schema": { "$ref": "#/components/schemas/Node" } }
            ]
        } } },
        "components": { "schemas": { "Node": { "type": "object" } } }
    });
    let api = OpenApi::new(&document, "https://api.example.com", http()).expect("document");
    let tools = tools(&mcp(api)).await;
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["node"],
        json!({ "$ref": "#/components/schemas/Node" })
    );
}

#[tokio::test]
async fn a_call_is_one_request_with_the_callers_token_and_encoded_arguments() {
    let upstream = upstream(200, "application/json", r#"{"orders":[]}"#).await;
    let server = mcp(api(&upstream.url("/v1/")).forward_caller_token());
    let arguments = json!({ "status": "OPEN NOW", "tag": ["a", "b"], "X-Tenant": "t1" });
    let result = call(&server, "listOrders", arguments).await;
    assert_eq!(result["structuredContent"], json!({ "orders": [] }));
    assert_eq!(text(&result), (r#"{"orders":[]}"#, false));
    let sent = only(&upstream);
    assert_eq!(sent.method, "GET");
    assert_eq!(sent.path, "/v1/orders?status=OPEN%20NOW&tag=a&tag=b");
    assert_eq!(sent.header("authorization"), Some("Bearer caller-token"));
    assert_eq!(sent.header("x-tenant"), Some("t1"));
    assert_eq!(sent.header("accept"), Some("application/json"));
    assert!(sent.body.is_empty());
}

#[tokio::test]
async fn a_flattened_body_is_sent_as_one_json_object() {
    let upstream = upstream(201, "application/json", r#"{"id":"o-1"}"#).await;
    let server = mcp(api(&upstream.url("")));
    let arguments = json!({ "item": "pen", "quantity": 2, "X-Tenant": "t1" });
    let result = call(&server, "create_order_", arguments).await;
    assert_eq!(text(&result), (r#"{"id":"o-1"}"#, false));
    let sent = only(&upstream);
    assert_eq!(
        (sent.method.as_str(), sent.path.as_str()),
        ("POST", "/orders")
    );
    assert_eq!(sent.header("content-type"), Some("application/json"));
    let body: Value = serde_json::from_slice(&sent.body).expect("JSON");
    assert_eq!(body, json!({ "item": "pen", "quantity": 2 }));
}

#[tokio::test]
async fn a_body_argument_is_sent_whole_and_path_values_are_encoded() {
    let upstream = upstream(200, "application/json", "[1,2]").await;
    let server = mcp(api(&upstream.url("")));
    let arguments = json!({ "id": "a/b 1", "body": { "note": "x" } });
    let result = call(&server, "replaceOrder", arguments).await;
    assert_eq!(text(&result), ("[1,2]", false));
    assert!(result.get("structuredContent").is_none());
    let sent = only(&upstream);
    assert_eq!(
        (sent.method.as_str(), sent.path.as_str()),
        ("PUT", "/orders/a%2Fb%201")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&sent.body).expect("JSON"),
        json!({ "note": "x" })
    );
}

#[tokio::test]
async fn an_optional_body_argument_left_out_sends_no_body() {
    let upstream = upstream(204, "application/json", "").await;
    let server = mcp(api(&upstream.url("")));
    let result = call(&server, "tagOrder", json!({ "id": 7 })).await;
    assert_eq!(text(&result), ("HTTP 204", false));
    let sent = only(&upstream);
    assert_eq!(
        (sent.method.as_str(), sent.path.as_str()),
        ("PATCH", "/orders/7")
    );
    assert!(sent.header("content-type").is_none());
}

#[tokio::test]
async fn missing_required_arguments_are_named_and_nothing_is_sent() {
    let upstream = upstream(200, "application/json", "{}").await;
    let server = mcp(api(&upstream.url("")));
    let result = call(&server, "listOrders", json!({ "X-Tenant": null })).await;
    assert_eq!(
        text(&result),
        (
            "ERROR_INVALID_ARGUMENTS: Missing required arguments: X-Tenant",
            true
        )
    );
    assert!(upstream.requests().is_empty());
}

#[tokio::test]
async fn an_argument_that_cannot_be_a_header_is_refused() {
    let upstream = upstream(200, "application/json", "{}").await;
    let server = mcp(api(&upstream.url("")));
    let result = call(&server, "listOrders", json!({ "X-Tenant": "a\nb" })).await;
    assert_eq!(
        text(&result),
        (
            "ERROR_INVALID_ARGUMENTS: X-Tenant cannot be sent as a header",
            true
        )
    );
    assert!(upstream.requests().is_empty());
}

#[tokio::test]
async fn a_caller_without_a_token_is_forwarded_without_one() {
    let upstream = upstream(200, "text/plain", "plain words").await;
    let server = mcp(api(&upstream.url("")));
    let params = json!({ "name": "deleteOrder", "arguments": { "id": "o-1" } });
    let response = server
        .handle(Arc::new(()), request(json!({}), "tools/call", params))
        .await;
    assert_eq!(
        text(&json_body(&response)["result"]),
        ("plain words", false)
    );
    assert!(only(&upstream).header("authorization").is_none());
}

#[tokio::test]
async fn a_4xx_answer_reaches_the_model_with_the_apis_own_message() {
    let upstream = upstream(
        404,
        "application/json",
        r#"{"errorCode":"ERROR_NOT_FOUND"}"#,
    )
    .await;
    let server = mcp(api(&upstream.url("")));
    let result = call(&server, "deleteOrder", json!({ "id": "o-1" })).await;
    let (message, error) = text(&result);
    assert!(error);
    assert!(message.starts_with(UPSTREAM_REFUSED), "{message}");
    assert!(
        message.contains("HTTP 404") && message.contains("ERROR_NOT_FOUND"),
        "{message}"
    );
}

#[tokio::test]
async fn a_5xx_or_a_redirect_is_reported_without_its_content() {
    for status in [500, 302] {
        let upstream = server::Server::fixed(status, "text/plain", "stack trace at 10.0.0.7").await;
        let server = mcp(api(&upstream.url("")));
        let result = call(&server, "deleteOrder", json!({ "id": "o-1" })).await;
        assert_eq!(
            text(&result),
            (
                format!("{UPSTREAM_UNAVAILABLE}: InternalServerError").as_str(),
                true
            )
        );
    }
}

#[tokio::test]
async fn an_answer_over_the_limit_is_refused() {
    let upstream = upstream(200, "application/json", r#"{"padding":"0123456789"}"#).await;
    let server = mcp(api(&upstream.url("")).response_limit(8));
    let result = call(&server, "deleteOrder", json!({ "id": "o-1" })).await;
    assert_eq!(
        text(&result),
        ("ERROR_LIMIT_EXCEEDED: InternalServerError", true)
    );
}

#[tokio::test]
async fn an_api_that_cannot_be_reached_is_unavailable() {
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("address")
    };
    let server = mcp(api(&format!("http://{closed}")));
    let result = call(&server, "deleteOrder", json!({ "id": "o-1" })).await;
    assert_eq!(
        text(&result),
        (
            format!("{UPSTREAM_UNAVAILABLE}: InternalServerError").as_str(),
            true
        )
    );
}

/// The API promises 100 bytes, sends 3 and hangs up.
#[tokio::test]
async fn an_answer_cut_short_is_unavailable() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).await;
        let head =
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n[1,";
        let _ = stream.write_all(head.as_bytes()).await;
    });
    let server = mcp(api(&format!("http://{address}")));
    let result = call(&server, "deleteOrder", json!({ "id": "o-1" })).await;
    assert_eq!(
        text(&result),
        (
            format!("{UPSTREAM_UNAVAILABLE}: InternalServerError").as_str(),
            true
        )
    );
}

#[tokio::test]
async fn an_api_slower_than_the_client_limit_is_a_timeout() {
    let upstream = server::Server::start(|_| async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        server::reply(200, "application/json", "{}")
    })
    .await;
    let limits = Limits::new(Duration::from_secs(1), Duration::from_millis(200));
    let slow = OpenApi::new(
        &document(),
        upstream.url(""),
        client::build(limits).expect("client"),
    )
    .expect("document");
    let result = call(&mcp(slow), "deleteOrder", json!({ "id": "o-1" })).await;
    assert_eq!(text(&result), ("ERROR_TIMEOUT: InternalServerError", true));
}

#[test]
fn a_document_describes_its_operations_for_debugging() {
    let text = format!("{:?}", api("https://api.example.com"));
    assert!(text.contains("listOrders"));
}

/// Forwarding the caller's token is a choice: without it, the API receives no
/// credential the caller sent.
#[tokio::test]
async fn by_default_the_callers_token_is_not_forwarded() {
    let upstream = upstream(200, "application/json", r#"{"orders":[]}"#).await;
    let server = mcp(api(&upstream.url("/v1/")));
    let arguments = json!({ "status": "OPEN", "tag": ["a"], "X-Tenant": "t1" });
    call(&server, "listOrders", arguments).await;
    assert!(only(&upstream).header("authorization").is_none());
}

/// The server's own credential goes on every call, a model-chosen argument
/// cannot replace it, and it never shows in `Debug` output.
#[tokio::test]
async fn a_configured_header_is_sent_and_arguments_cannot_replace_it() {
    let upstream = upstream(200, "application/json", r#"{"orders":[]}"#).await;
    let api = api(&upstream.url("/v1/")).header(
        HeaderName::from_static("x-tenant"),
        HeaderValue::from_static("service-secret"),
    );
    assert!(!format!("{api:?}").contains("service-secret"));
    let server = mcp(api);
    call(
        &server,
        "listOrders",
        json!({ "status": "OPEN", "X-Tenant": "t1" }),
    )
    .await;
    let sent = only(&upstream);
    let values: Vec<&str> = sent
        .headers
        .get_all("x-tenant")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    assert_eq!(values, ["service-secret"]);
    assert!(sent.header("authorization").is_none());
}
