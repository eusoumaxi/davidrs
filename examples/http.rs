//! A public JSON endpoint: `POST {"name": "..."}` answers a greeting.
//! Runs under Cargo Lambda with no AWS resources.

use std::sync::Arc;

use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request};
use davidrs::{Context, RuntimeError};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Input {
    name: String,
}

#[derive(Serialize)]
struct Greeting {
    message: String,
}

async fn hello(_: Arc<()>, input: Input, _: Context<()>) -> Result<Json<Greeting>, Failure> {
    Ok(Json(Greeting {
        message: format!("Hello, {}", input.name),
    }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    Api::new("hello", Public, PlainErrors)
        .run(
            Arc::new(()),
            |request: &Request<'_>| request.json::<Input>(),
            hello,
        )
        .await
}
