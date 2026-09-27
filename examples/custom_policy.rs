//! An application's own policy and error renderer: the endpoint answers only
//! `GET`, and failures are plain text. Runs under Cargo Lambda with no AWS
//! resources.

use std::sync::Arc;

use davidrs::http::{
    literal, Api, ErrorRenderer, Failure, HttpResponse, Json, Method, Policy, Request, StatusCode,
};
use davidrs::{Context, Invocation, RuntimeError};

/// Refuses every method but `GET` with a `405` that names the allowed one.
struct ReadOnly;

impl Policy for ReadOnly {
    type Scope = ();

    async fn authorize(&self, request: &Request<'_>, _: &Invocation) -> Result<(), Failure> {
        if request.method() != Method::GET {
            return Err(
                Failure::new(StatusCode::METHOD_NOT_ALLOWED, "READ_ONLY", "Use GET")
                    .with_header("allow", "GET"),
            );
        }
        Ok(())
    }
}

/// Renders the public message as plain text and keeps the failure's headers.
struct TextErrors;

impl ErrorRenderer for TextErrors {
    fn render(&self, failure: &Failure) -> HttpResponse {
        let mut response = literal(
            failure.status(),
            "text/plain",
            failure.public_message().to_owned(),
        );
        for (name, value) in failure.headers() {
            response.headers_mut().insert(name.clone(), value.clone());
        }
        response
    }
}

async fn ready(_: Arc<()>, _: (), _: Context<()>) -> Result<Json<serde_json::Value>, Failure> {
    Ok(Json(serde_json::json!({"ready": true})))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    Api::new("ready", ReadOnly, TextErrors)
        .run(Arc::new(()), |_| Ok(()), ready)
        .await
}
