//! Strict JSON input checked with Garde, with failures rendered as RFC 9457
//! problem details. Runs under Cargo Lambda with no AWS resources.

use std::sync::Arc;

use davidrs::http::{Api, Failure, Json, ProblemErrors, Public, Request};
use davidrs::{Context, RuntimeError};
use serde::{Deserialize, Serialize};

/// Unknown fields are refused, and the name must be 1 to 100 characters.
#[derive(Deserialize, Serialize, garde::Validate)]
#[serde(deny_unknown_fields)]
struct Input {
    #[garde(length(chars, min = 1, max = 100))]
    name: String,
}

async fn echo(_: Arc<()>, input: Input, _: Context<()>) -> Result<Json<Input>, Failure> {
    Ok(Json(input))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    Api::new("echo", Public, ProblemErrors::default())
        .run(
            Arc::new(()),
            |request: &Request<'_>| request.validated_json::<Input>(),
            echo,
        )
        .await
}
