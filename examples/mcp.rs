//! An MCP server: one hand-written tool and the read operations of an OpenAPI
//! document, for callers an API Gateway JWT authorizer verified. Runs under
//! Cargo Lambda with `API_URL`, `MCP_URL` and `ISSUER` set.

use std::sync::Arc;

use davidrs::client::{self, Limits};
use davidrs::http::access::{Access, Claims, Grant};
use davidrs::http::Failure;
use davidrs::mcp::openapi::OpenApi;
use davidrs::mcp::{ProtectedResource, Server, Tool};
use davidrs::{required_env, Context, RuntimeError};
use serde_json::{json, Value};

/// The API the OpenAPI tools call.
const DOCUMENT: &str = r#"{
  "openapi": "3.1.0",
  "paths": {
    "/orders": {
      "get": { "operationId": "listOrders", "summary": "List your orders" }
    },
    "/orders/{id}": {
      "get": {
        "operationId": "getOrder",
        "summary": "Read one order",
        "parameters": [{ "name": "id", "in": "path", "schema": { "type": "string" } }]
      },
      "delete": { "operationId": "deleteOrder", "summary": "Delete one order" }
    }
  }
}"#;

/// The caller, from the authorizer's claims.
struct User {
    id: String,
}

fn user(claims: &Claims) -> Option<User> {
    Some(User {
        id: claims.subject()?.to_owned(),
    })
}

async fn whoami(
    _: Arc<()>,
    _: Value,
    context: Context<Grant<User, ()>>,
) -> Result<String, Failure> {
    Ok(format!("You are {}", context.scope().caller().id))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let document: Value = serde_json::from_str(DOCUMENT)
        .map_err(|error| RuntimeError::other("reading the OpenAPI document", error))?;
    let api = OpenApi::new(
        &document,
        required_env("API_URL")?,
        client::build(Limits::default())?,
    )?
    .select(|operation| operation.method() == "GET")
    .forward_caller_token();
    let resource = ProtectedResource::new(required_env("MCP_URL")?, [required_env("ISSUER")?]);
    let no_arguments = json!({ "type": "object", "additionalProperties": false });

    Server::new("orders", "1.0.0", Access::new(user).require_caller())
        .instructions("Read-only access to the caller's orders.")
        .protected_resource(resource)
        .tool(Tool::new(
            "whoami",
            "Says who the caller is",
            no_arguments,
            whoami,
        ))
        .openapi(api)
        .run(Arc::new(()))
        .await
}
