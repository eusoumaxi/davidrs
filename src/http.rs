//! The buffered HTTP pipeline for API Gateway and Function URL requests.
//!
//! One ordered pipeline serves one route; this is not a web framework. See
//! [`Api`] for the steps, [`Request`] for bounded reads, [`Failure`] for safe
//! public error messages and [`ErrorRenderer`] for the wire shape of errors. HTTP
//! API and Function URL events are read by default; REST API events
//! need the `apigw-rest` feature.
//!
//! # Examples
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request};
//! use davidrs::{Context, RuntimeError};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug)]
//! struct App {
//!     greeting: String,
//! }
//!
//! #[derive(Debug, Deserialize)]
//! struct HelloPath {
//!     name: String,
//! }
//!
//! #[derive(Debug, Serialize)]
//! struct Greeting {
//!     message: String,
//! }
//!
//! async fn hello(app: Arc<App>, input: HelloPath, _context: Context<()>) -> Result<Json<Greeting>, Failure> {
//!     Ok(Json(Greeting { message: format!("{}, {}", app.greeting, input.name) }))
//! }
//!
//! # async fn run() -> Result<(), RuntimeError> {
//! let app = Arc::new(App { greeting: "Hello".to_owned() });
//! let operation = "hello";
//!
//! Api::new(operation, Public, PlainErrors::default())
//!     .run(app, |request: &Request| request.path::<HelloPath>(), hello)
//!     .await
//! # }
//! ```

mod api;
mod failure;
mod policy;
mod render;
mod request;
mod response;

pub mod access;
pub mod codes;
pub mod fields;
pub mod rate_limit;
pub mod schema;
#[cfg(feature = "http-stream")]
pub mod stream;

pub use api::{Api, Finalizer};
pub use failure::{ErrorCatalog, ErrorDefinition, Failure, FailureKind, INTERNAL_MESSAGE};
pub use policy::{Admission, AdmitAll, Policy, Public};
pub use rate_limit::{Counter, RateLimit, RateLimitConfig, RateLimited};
pub use render::{ErrorRenderer, PlainErrors, literal};
pub use request::{DEFAULT_BODY_LIMIT, Request};
pub use response::{HttpResponse, IntoResponse, Json, NoContent};

pub use lambda_http::Body;
pub use lambda_http::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};

#[cfg(feature = "problem")]
pub use render::ProblemErrors;
