//! A tool: its definition as `tools/list` shows it, and the handler a
//! `tools/call` runs.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::http::{Failure, FailureKind, StatusCode, codes};
use crate::{Context, RuntimeError};

/// The code of a tool call whose arguments do not fit the tool: a typed
/// argument that does not deserialize, or a required argument that is
/// missing.
pub const INVALID_ARGUMENTS: &str = "ERROR_INVALID_ARGUMENTS";

/// What every tool's handler becomes once its argument and result types are
/// erased. The last argument is the caller's `Authorization` header, which
/// only forwarding tools read.
pub(super) type Run<App, Scope> = Box<
    dyn Fn(
            Arc<App>,
            Map<String, Value>,
            Context<Scope>,
            Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send>>
        + Send
        + Sync,
>;

/// One tool: a name, a description a model reads, the JSON Schema of its
/// arguments, and the handler a call runs.
///
/// The handler has the shape of an HTTP handler: the application state, the
/// arguments deserialized into `A`, and a [`Context`] whose scope is what the
/// server's policy established and whose deadline already keeps
/// [`CALL_MARGIN`](super::CALL_MARGIN) back. Its success is serialized into
/// the result; its [`Failure`] becomes a result with `isError: true` that a
/// model can read and act on, with a 5xx message redacted as everywhere else.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
///
/// use davidrs::http::Failure;
/// use davidrs::mcp::Tool;
/// use davidrs::Context;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Deserialize)]
/// struct Add {
///     a: i64,
///     b: i64,
/// }
///
/// #[derive(Serialize)]
/// struct Sum {
///     sum: i64,
/// }
///
/// async fn add(_app: Arc<()>, input: Add, _context: Context<()>) -> Result<Sum, Failure> {
///     Ok(Sum { sum: input.a + input.b })
/// }
///
/// let schema = serde_json::json!({
///     "type": "object",
///     "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
///     "required": ["a", "b"]
/// });
/// let tool = Tool::new("add", "Adds two integers", schema, add);
/// assert_eq!(tool.name(), "add");
/// ```
pub struct Tool<App, Scope> {
    pub(super) definition: Value,
    pub(super) run: Run<App, Scope>,
}

impl<App, Scope> Tool<App, Scope> {
    /// A tool whose calls run `handler`.
    ///
    /// `input_schema` is published as the tool's `inputSchema`; describe
    /// exactly what `A` accepts, since the model writes its arguments from it.
    /// Arguments that do not deserialize into `A` are refused with
    /// [`INVALID_ARGUMENTS`] before the handler runs. A success that
    /// serializes to an object is also returned as `structuredContent`; a
    /// string is returned as plain text.
    #[must_use]
    pub fn new<A, T, H, F>(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: H,
    ) -> Self
    where
        A: DeserializeOwned,
        T: Serialize,
        H: Fn(Arc<App>, A, Context<Scope>) -> F + Send + Sync + 'static,
        F: Future<Output = Result<T, Failure>> + Send + 'static,
    {
        let definition = json!({
            "name": name.into(),
            "description": description.into(),
            "inputSchema": input_schema,
        });
        let run: Run<App, Scope> = Box::new(move |app, arguments, context, _authorization| {
            match serde_json::from_value::<A>(Value::Object(arguments)) {
                Ok(input) => {
                    let work = handler(app, input, context);
                    Box::pin(
                        async move { serde_json::to_value(work.await?).map_err(serialization) },
                    )
                }
                Err(error) => {
                    let failure = Failure::new(
                        StatusCode::BAD_REQUEST,
                        INVALID_ARGUMENTS,
                        format!("Invalid arguments: {error}"),
                    );
                    Box::pin(std::future::ready(Err(failure)))
                }
            }
        });
        Self { definition, run }
    }

    /// A tool with a definition and an erased handler, as the OpenAPI source
    /// builds them.
    #[cfg(feature = "mcp-openapi")]
    pub(super) fn from_parts(definition: Value, run: Run<App, Scope>) -> Self {
        Self { definition, run }
    }

    /// Publishes behaviour hints, such as `{"readOnlyHint": true}`.
    ///
    /// Clients treat annotations as untrusted hints, for example to skip a
    /// confirmation before a read-only call. They never replace a permission
    /// check in the handler.
    #[must_use]
    pub fn annotations(mut self, annotations: Value) -> Self {
        self.definition["annotations"] = annotations;
        self
    }

    /// The tool's name.
    pub fn name(&self) -> &str {
        self.definition["name"].as_str().unwrap_or_default()
    }
}

impl<App, Scope> fmt::Debug for Tool<App, Scope> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tool")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

/// A success value that could not be serialized: a `500`, never a result
/// that silently lost its content.
fn serialization(error: serde_json::Error) -> Failure {
    Failure::from_error(codes::SERIALIZATION, &error).with_kind(FailureKind::Serialization)
}

/// The `tools/call` result for what a handler produced within its deadline.
///
/// A deadline that ran out reads as [`codes::TIMEOUT`].
pub(super) fn result(
    tool: &str,
    outcome: Result<Result<Value, Failure>, RuntimeError>,
) -> Map<String, Value> {
    match outcome
        .map_err(Failure::from)
        .and_then(std::convert::identity)
    {
        Ok(value) => {
            let text = match &value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let mut result = content(text, false);
            if value.is_object() {
                result.insert("structuredContent".into(), value);
            }
            result
        }
        Err(failure) => {
            log(tool, &failure);
            let text = format!("{}: {}", failure.code(), failure.public_message());
            content(text, true)
        }
    }
}

/// A result with one text block.
fn content(text: String, error: bool) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("content".into(), json!([{ "type": "text", "text": text }]));
    result.insert("isError".into(), json!(error));
    result
}

/// Logs a failed call's safe metadata, as the HTTP pipeline does for a
/// failed request: never the message or the detail.
fn log(tool: &str, failure: &Failure) {
    #[cfg(feature = "logs")]
    {
        if failure.is_server_error() {
            tracing::error!(tool, code = failure.code(), kind = ?failure.kind(), "tool failed");
        } else {
            tracing::debug!(tool, code = failure.code(), "tool refused");
        }
    }
    #[cfg(not(feature = "logs"))]
    {
        let _ = (tool, failure);
    }
}
