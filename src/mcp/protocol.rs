//! JSON-RPC over Streamable HTTP: reading one message, checking the request
//! metadata the protocol requires, and shaping the replies.
//!
//! Every request names its protocol revision and its client capabilities in
//! `params._meta`, and mirrors the revision, the method and a tool's name
//! into headers that must agree with the body.

use base64::Engine as _;
use lambda_http::Body;
use lambda_http::http::{HeaderMap, HeaderValue, StatusCode, header};
use serde_json::{Map, Value, json};

use crate::http::HttpResponse;

/// The protocol revision this server speaks: MCP 2026-07-28.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

pub(super) const PARSE_ERROR: i64 = -32700;
pub(super) const INVALID_REQUEST: i64 = -32600;
pub(super) const METHOD_NOT_FOUND: i64 = -32601;
pub(super) const INVALID_PARAMS: i64 = -32602;
pub(super) const INTERNAL_ERROR: i64 = -32603;
const HEADER_MISMATCH: i64 = -32020;
const UNSUPPORTED_VERSION: i64 = -32022;

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
const VERSION_HEADER: &str = "MCP-Protocol-Version";

/// One JSON-RPC request or notification, as it arrived.
#[derive(Debug)]
pub(super) struct Message {
    /// `None` for a notification.
    pub(super) id: Option<Value>,
    pub(super) method: String,
    pub(super) params: Map<String, Value>,
}

impl Message {
    /// One string parameter, such as a tool's `name`.
    pub(super) fn text(&self, name: &str) -> Option<&str> {
        self.params.get(name).and_then(Value::as_str)
    }
}

/// A finished answer: a status and, except for an accepted notification, a
/// JSON body.
#[derive(Debug)]
pub(super) struct Reply {
    status: StatusCode,
    body: Option<Value>,
}

impl Reply {
    /// `202` with no body: an accepted notification.
    pub(super) fn accepted() -> Self {
        Self {
            status: StatusCode::ACCEPTED,
            body: None,
        }
    }

    /// A JSON-RPC error response; `id` is absent when the request's own id
    /// could not be read.
    pub(super) fn error(
        status: StatusCode,
        id: Option<Value>,
        code: i64,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        let mut error = json!({ "code": code, "message": message.into() });
        if let Some(data) = data {
            error["data"] = data;
        }
        let mut body = json!({ "jsonrpc": "2.0", "error": error });
        if let Some(id) = id {
            body["id"] = id;
        }
        Self {
            status,
            body: Some(body),
        }
    }

    /// A `200` complete result, with the server's identity in `_meta`.
    pub(super) fn result(id: Value, server: &Value, mut result: Map<String, Value>) -> Self {
        result.insert("resultType".into(), json!("complete"));
        result.insert("_meta".into(), json!({ META_SERVER_INFO: server }));
        Self {
            status: StatusCode::OK,
            body: Some(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
        }
    }

    /// The Lambda response.
    pub(super) fn into_response(self) -> HttpResponse {
        let mut response = HttpResponse::new(Body::Empty);
        if let Some(body) = self.body {
            *response.body_mut() = Body::Text(body.to_string());
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }
        *response.status_mut() = self.status;
        response
    }
}

/// Reads the body as one JSON-RPC message.
///
/// # Errors
///
/// Returns the `400` reply (`-32700` or `-32600`) for a body that is not one
/// request or notification. An `id` of `null` is such a body: MCP forbids it
/// in a request, and a notification has no `id` member at all.
pub(super) fn parse(body: &[u8]) -> Result<Message, Reply> {
    let invalid = |id, message: &str| {
        Reply::error(StatusCode::BAD_REQUEST, id, INVALID_REQUEST, message, None)
    };
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        Reply::error(
            StatusCode::BAD_REQUEST,
            None,
            PARSE_ERROR,
            "Parse error",
            None,
        )
    })?;
    let Value::Object(mut object) = value else {
        return Err(invalid(None, "A single JSON-RPC message is required"));
    };
    let id = object.remove("id");
    if id
        .as_ref()
        .is_some_and(|id| !(id.is_string() || id.is_number()))
    {
        return Err(invalid(None, "id must be a string or a number"));
    }
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid(id, "jsonrpc must be \"2.0\""));
    }
    let method = match object.remove("method") {
        Some(Value::String(method)) if !method.is_empty() => method,
        _ => return Err(invalid(id, "method is required")),
    };
    let params = match object.remove("params") {
        Some(Value::Object(params)) => params,
        _ => Map::new(),
    };
    Ok(Message { id, method, params })
}

/// Checks the metadata every request carries, in this order:
///
/// 1. the revision it names, in `_meta` or else in the header, is
///    [`PROTOCOL_VERSION`]: `-32022`, listing the supported revision;
/// 2. `_meta` names the revision and the client capabilities: `-32602`;
/// 3. the `MCP-Protocol-Version`, `Mcp-Method` and, for `tools/call`,
///    `Mcp-Name` headers repeat the body: `-32020`.
///
/// A proxy that routed on a header therefore never disagrees with what the
/// server executes.
///
/// # Errors
///
/// Returns the `400` reply for the first check that fails.
pub(super) fn validate(headers: &HeaderMap, message: &Message) -> Result<(), Reply> {
    let reject = |code, text: String, data| {
        Err(Reply::error(
            StatusCode::BAD_REQUEST,
            message.id.clone(),
            code,
            text,
            data,
        ))
    };
    let meta = message.params.get("_meta").and_then(Value::as_object);
    let declared = meta
        .and_then(|meta| meta.get(META_VERSION))
        .and_then(Value::as_str);
    let requested = declared.or_else(|| header_text(headers, VERSION_HEADER));
    if requested != Some(PROTOCOL_VERSION) {
        return reject(
            UNSUPPORTED_VERSION,
            "Unsupported protocol version".to_owned(),
            Some(json!({ "supported": [PROTOCOL_VERSION], "requested": requested })),
        );
    }
    let capabilities = meta.and_then(|meta| meta.get(META_CAPABILITIES));
    if declared.is_none() || !capabilities.is_some_and(Value::is_object) {
        return reject(
            INVALID_PARAMS,
            format!("_meta must name {META_VERSION} and {META_CAPABILITIES}"),
            None,
        );
    }
    let mut mirrors = vec![
        (VERSION_HEADER, PROTOCOL_VERSION),
        ("Mcp-Method", message.method.as_str()),
    ];
    if message.method == "tools/call" {
        mirrors.push(("Mcp-Name", message.text("name").unwrap_or_default()));
    }
    for (name, expected) in mirrors {
        if header_text(headers, name).and_then(decode).as_deref() != Some(expected) {
            return reject(
                HEADER_MISMATCH,
                format!(
                    "Header mismatch: {name} is missing or differs from the body value '{expected}'"
                ),
                None,
            );
        }
    }
    Ok(())
}

/// One header as trimmed text.
fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
}

/// A header value with the `=?base64?…?=` form decoded, which a client uses
/// for values that are not plain ASCII.
fn decode(value: &str) -> Option<String> {
    match value
        .strip_prefix("=?base64?")
        .and_then(|rest| rest.strip_suffix("?="))
    {
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok()),
        None => Some(value.to_owned()),
    }
}
