//! Streamed HTTP: one handler answering JSON or server-sent events through a
//! Lambda response stream — a Function URL in `RESPONSE_STREAM` mode or a
//! REST API method with the `STREAM` integration.
//!
//! [`StreamApi`] is the streamed twin of [`Api`](super::Api). One invocation
//! runs these steps, and every failure, from any step, reaches the same
//! [`ErrorRenderer`]:
//!
//! 1. **read** the invocation payload as an HTTP request (`400` if it is not one)
//! 2. **prepare** the request, when a hook is configured
//! 3. **CORS**: a browser `Origin` outside the allowlist is a `403`; `OPTIONS`
//!    is answered as a preflight
//! 4. **method**: anything the endpoint does not serve is a `405`
//! 5. **negotiate**: `Accept` chooses JSON or `text/event-stream`
//! 6. **policy**: the scope the handler runs with
//! 7. **handler**: returns the response head and a body, [`json`] for one
//!    document or [`events`] for a stream the response owns
//!
//! Steps 5–7 share the invocation deadline minus a margin (one second by
//! default) that keeps time to flush the last frames. Every response, success
//! or failure, gets `Vary: Origin, Accept`, the configured finalizer and, for
//! an allowed origin, the CORS headers.
//!
//! Decoding and admission are the handler's: a streamed endpoint often
//! decides what to count only after it knows the caller (an anonymous
//! request spends a rate-limit budget a signed-in one does not), which a
//! stage before the policy could not express.
//!
//! # Examples
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! use davidrs::http::stream::{self, Cors, StreamApi, StreamRequest, StreamResponse};
//! use davidrs::http::{Failure, PlainErrors, Public, StatusCode};
//! use davidrs::{Context, RuntimeError};
//!
//! async fn count(
//!     _app: Arc<()>,
//!     request: StreamRequest,
//!     context: Context<()>,
//! ) -> Result<StreamResponse, Failure> {
//!     if !request.wants_events() {
//!         return Ok(stream::json(StatusCode::OK, &serde_json::json!({"count": 3})));
//!     }
//!     let deadline = context.deadline().with_margin(Duration::from_secs(1));
//!     Ok(stream::events(deadline, |events| async move {
//!         for n in 1..=3 {
//!             let frame = stream::sse_frame(None, Some("count"), &n.to_string());
//!             if events.send(frame).await.is_err() {
//!                 return;
//!             }
//!         }
//!     }))
//! }
//!
//! # async fn run() -> Result<(), RuntimeError> {
//! StreamApi::new("count", Public, PlainErrors)
//!     .cors(Cors::new(vec!["https://app.example.com".to_owned()]))
//!     .run(Arc::new(()), count)
//!     .await
//! # }
//! ```

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use lambda_http::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use lambda_http::RequestExt as _;
use lambda_runtime::streaming::Body;
use lambda_runtime::{LambdaEvent, MetadataPrelude};
use serde_json::Value;

use super::codes;
use super::failure::{Failure, FailureKind};
use super::policy::Policy;
use super::render::ErrorRenderer;
use super::request::Request;
use super::response::HttpResponse;
use crate::streaming::{Producer, StreamBody};
use crate::{Context, Deadline, Invocation, RuntimeError};

/// A streamed response: the head (status, headers) and a body stream.
pub type StreamResponse = lambda_runtime::StreamResponse<Body>;

/// Changes the request before anything reads it — for a header an edge
/// function relocated, say. Runs once per invocation.
pub type Prepare = Box<dyn Fn(&mut lambda_http::Request) + Send + Sync>;

/// Adds the headers a service puts on every streamed response.
pub type HeaderFinalizer = Box<dyn Fn(&Invocation, &mut HeaderMap) + Send + Sync>;

/// The media type of a server-sent event stream.
pub const EVENT_STREAM: &str = "text/event-stream; charset=utf-8";

/// How long one frame may wait for a slow reader before the stream gives up.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// Frames a producer may run ahead of the reader.
const STREAM_CAPACITY: usize = 4;

/// The representation the client accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Representation {
    /// One `application/json` document.
    Json,
    /// `text/event-stream`: frames as the work progresses.
    EventStream,
}

/// Chooses JSON or an event stream from an `Accept` header.
///
/// Each representation takes the quality of the most specific range that
/// matches it (`text/event-stream` over `text/*` over `*/*`), so
/// `text/event-stream;q=0, */*` excludes the stream. The higher quality wins;
/// JSON wins a tie, so an absent header or `*/*` means JSON. Media types and
/// the `q` name are case-insensitive.
///
/// # Errors
///
/// A `400` ([`codes::INVALID_ACCEPT`]) for an unreadable quality, a `406`
/// ([`codes::NOT_ACCEPTABLE`]) when both representations are excluded.
///
/// # Examples
///
/// ```
/// use davidrs::http::stream::{negotiate, Representation};
///
/// assert_eq!(negotiate(None).unwrap(), Representation::Json);
/// assert_eq!(
///     negotiate(Some("application/json;q=0.5, text/event-stream")).unwrap(),
///     Representation::EventStream
/// );
/// ```
pub fn negotiate(accept: Option<&str>) -> Result<Representation, Failure> {
    let Some(accept) = accept else {
        return Ok(Representation::Json);
    };
    let mut json = (0, 0_f32);
    let mut stream = (0, 0_f32);
    for range in accept.split(',') {
        let mut parts = range.trim().split(';');
        let media = parts.next().unwrap_or("").trim();
        let mut quality = 1_f32;
        for parameter in parts {
            let weight = parameter
                .trim()
                .split_once('=')
                .filter(|(name, _)| name.eq_ignore_ascii_case("q"));
            if let Some((_, q)) = weight {
                quality = q
                    .parse()
                    .ok()
                    .filter(|q: &f32| q.is_finite() && (0.0..=1.0).contains(q))
                    .ok_or_else(|| {
                        Failure::new(
                            StatusCode::BAD_REQUEST,
                            codes::INVALID_ACCEPT,
                            "Invalid Accept quality",
                        )
                        .with_kind(FailureKind::Decode)
                    })?;
            }
        }
        for (target, exact, group) in [
            (&mut json, "application/json", "application/*"),
            (&mut stream, "text/event-stream", "text/*"),
        ] {
            let specificity = if media.eq_ignore_ascii_case(exact) {
                3
            } else if media.eq_ignore_ascii_case(group) {
                2
            } else if media == "*/*" {
                1
            } else {
                0
            };
            if specificity > 0
                && (specificity > target.0 || (specificity == target.0 && quality > target.1))
            {
                *target = (specificity, quality);
            }
        }
    }
    if json.1 == 0.0 && stream.1 == 0.0 {
        return Err(Failure::new(
            StatusCode::NOT_ACCEPTABLE,
            codes::NOT_ACCEPTABLE,
            "Accept application/json or text/event-stream",
        )
        .with_kind(FailureKind::Decode));
    }
    Ok(if stream.1 > json.1 {
        Representation::EventStream
    } else {
        Representation::Json
    })
}

/// One JSON document as a streamed response. A `204` has no body.
#[must_use]
pub fn json(status: StatusCode, value: &Value) -> StreamResponse {
    let body = if status == StatusCode::NO_CONTENT {
        Body::empty()
    } else {
        Body::from(value.to_string())
    };
    head(status, "application/json", body)
}

/// An event stream whose producer the response owns.
///
/// `produce` runs as its own task and writes frames through the
/// [`EventWriter`], at most four frames ahead of the reader. Dropping the
/// response (the client left) cancels it, and `deadline` bounds it even while
/// a client keeps reading.
///
/// # Panics
///
/// Outside a Tokio runtime, as [`tokio::spawn`] does.
pub fn events<F, Fut>(deadline: Deadline, produce: F) -> StreamResponse
where
    F: FnOnce(EventWriter) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    use futures_util::TryStreamExt as _;
    let chunks = StreamBody::spawn(STREAM_CAPACITY, deadline, |producer| {
        produce(EventWriter { producer })
    });
    let body = Body::new(http_body_util::StreamBody::new(
        chunks.map_ok(http_body::Frame::data),
    ));
    head(StatusCode::OK, EVENT_STREAM, body)
}

/// A buffered response, typically a rendered failure, as a streamed one.
#[must_use]
pub fn from_response(response: HttpResponse) -> StreamResponse {
    let (parts, body) = response.into_parts();
    let bytes: &[u8] = body.as_ref();
    let body = if bytes.is_empty() {
        Body::empty()
    } else {
        Body::from(bytes.to_vec())
    };
    StreamResponse {
        metadata_prelude: MetadataPrelude {
            status_code: parts.status,
            headers: parts.headers,
            ..MetadataPrelude::default()
        },
        stream: body,
    }
}

fn head(status: StatusCode, content_type: &'static str, body: Body) -> StreamResponse {
    let mut metadata = MetadataPrelude {
        status_code: status,
        ..MetadataPrelude::default()
    };
    metadata
        .headers
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    StreamResponse {
        metadata_prelude: metadata,
        stream: body,
    }
}

/// One server-sent event frame: optional `id:` and `event:` lines, one
/// `data:` line per line of `data`, and the blank line that ends it.
///
/// `data` may break lines with `\n`, `\r\n` or a lone `\r`, as the event
/// stream format allows; each becomes its own `data:` line, so no part of the
/// data is read by the client as another field. `id` and `event` are one line
/// each: any line break in them is dropped, so a value taken from a request
/// (a cursor echoed as the id, say) cannot add a field or end the frame.
///
/// ```
/// use davidrs::http::stream::sse_frame;
/// assert_eq!(sse_frame(Some("1"), Some("snapshot"), "{}"), "id: 1\nevent: snapshot\ndata: {}\n\n");
/// assert_eq!(sse_frame(None, None, "a\nb"), "data: a\ndata: b\n\n");
/// assert_eq!(sse_frame(Some("1\ndata: x"), None, "{}"), "id: 1data: x\ndata: {}\n\n");
/// ```
#[must_use]
pub fn sse_frame(id: Option<&str>, event: Option<&str>, data: &str) -> String {
    let mut frame = String::with_capacity(data.len() + 32);
    for (field, value) in [("id: ", id), ("event: ", event)] {
        if let Some(value) = value {
            frame.push_str(field);
            frame.extend(value.chars().filter(|c| !matches!(c, '\r' | '\n')));
            frame.push('\n');
        }
    }
    for line in data.split("\r\n").flat_map(|part| part.split(['\r', '\n'])) {
        frame.push_str("data: ");
        frame.push_str(line);
        frame.push('\n');
    }
    frame.push('\n');
    frame
}

/// Where an event-stream producer writes its frames.
#[derive(Debug)]
pub struct EventWriter {
    producer: Producer,
}

impl EventWriter {
    /// Sends one complete frame (see [`sse_frame`]).
    ///
    /// # Errors
    ///
    /// Fails when the client is gone or has not read for a second: either way
    /// the producer should stop.
    pub async fn send(&self, frame: impl Into<Bytes>) -> Result<(), RuntimeError> {
        match tokio::time::timeout(SEND_TIMEOUT, self.producer.send(frame)).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(RuntimeError::message("the event stream was closed")),
            Err(_) => Err(RuntimeError::message("the event stream reader stalled")),
        }
    }

    /// The owned producer: its deadline, and whether it should stop.
    #[must_use]
    pub fn producer(&self) -> &Producer {
        &self.producer
    }
}

/// CORS for browser callers: which origins may read the response, and what
/// they may send and see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cors {
    origins: Vec<String>,
    allow_headers: String,
    expose_headers: String,
    max_age: Duration,
}

impl Cors {
    /// Allows exactly these origins (`https://app.example.com`, no path).
    ///
    /// The list comes from configuration, never from a request: a response
    /// that echoes whatever `Origin` arrived is readable by every site.
    #[must_use]
    pub fn new(origins: Vec<String>) -> Self {
        Self {
            origins,
            allow_headers: "accept,authorization,content-type".to_owned(),
            expose_headers: String::new(),
            max_age: Duration::from_secs(600),
        }
    }

    /// The request headers a browser may send (`Access-Control-Allow-Headers`).
    #[must_use]
    pub fn allow_headers(mut self, headers: &str) -> Self {
        self.allow_headers = headers.to_owned();
        self
    }

    /// The response headers a browser may read (`Access-Control-Expose-Headers`).
    #[must_use]
    pub fn expose_headers(mut self, headers: &str) -> Self {
        self.expose_headers = headers.to_owned();
        self
    }

    /// How long a browser may cache a preflight (`Access-Control-Max-Age`);
    /// ten minutes by default.
    ///
    /// The header carries whole seconds, so a fraction of a second is dropped.
    #[must_use]
    pub fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// Whether `origin` is on the allowlist.
    #[must_use]
    pub fn allows(&self, origin: &str) -> bool {
        self.origins.iter().any(|allowed| allowed == origin)
    }

    fn apply(&self, origin: &str, methods: &str, headers: &mut HeaderMap) {
        let values = [
            ("access-control-allow-origin", origin),
            ("access-control-allow-methods", methods),
            ("access-control-allow-headers", self.allow_headers.as_str()),
            (
                "access-control-expose-headers",
                self.expose_headers.as_str(),
            ),
        ];
        for (name, value) in values {
            if value.is_empty() {
                continue;
            }
            if let Ok(value) = HeaderValue::try_from(value) {
                headers.insert(HeaderName::from_static(name), value);
            }
        }
        headers.insert(
            HeaderName::from_static("access-control-max-age"),
            HeaderValue::from(self.max_age.as_secs()),
        );
    }
}

/// What a streamed handler receives: the request, the payload as Lambda
/// delivered it, and the representation negotiated for the response.
#[derive(Debug)]
pub struct StreamRequest {
    native: lambda_http::Request,
    event: Value,
    representation: Representation,
}

impl StreamRequest {
    /// The typed request.
    #[must_use]
    pub fn native(&self) -> &lambda_http::Request {
        &self.native
    }

    /// The bounded, typed readers over the request: query, path, headers.
    #[must_use]
    pub fn view(&self) -> Request<'_> {
        Request::new(&self.native)
    }

    /// The path as the client sent it, without the stage prefix.
    #[must_use]
    pub fn path(&self) -> &str {
        self.native.raw_http_path()
    }

    /// The invocation payload exactly as the platform delivered it.
    ///
    /// Use it for what the typed request derives differently, such as a
    /// Function URL's own decoded `queryStringParameters`: the typed request
    /// parses the raw query string again, under WHATWG rules.
    #[must_use]
    pub fn event(&self) -> &Value {
        &self.event
    }

    /// The negotiated representation.
    #[must_use]
    pub fn representation(&self) -> Representation {
        self.representation
    }

    /// Whether the client asked for an event stream.
    #[must_use]
    pub fn wants_events(&self) -> bool {
        self.representation == Representation::EventStream
    }
}

/// A configured streamed endpoint: one operation, one policy, one renderer.
pub struct StreamApi<P, R> {
    operation: &'static str,
    policy: P,
    renderer: R,
    methods: Vec<Method>,
    cors: Option<Cors>,
    margin: Duration,
    prepare: Option<Prepare>,
    finalize: Option<HeaderFinalizer>,
}

impl<P, R> std::fmt::Debug for StreamApi<P, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamApi")
            .field("operation", &self.operation)
            .field("methods", &self.methods)
            .field("cors", &self.cors)
            .field("margin", &self.margin)
            .finish_non_exhaustive()
    }
}

impl<P, R> StreamApi<P, R>
where
    P: Policy,
    R: ErrorRenderer,
{
    /// Configures an endpoint serving `GET`, without CORS, with a one-second
    /// margin. Performs no I/O.
    #[must_use]
    pub fn new(operation: &'static str, policy: P, renderer: R) -> Self {
        Self {
            operation,
            policy,
            renderer,
            methods: vec![Method::GET],
            cors: None,
            margin: Duration::from_secs(1),
            prepare: None,
            finalize: None,
        }
    }

    /// The methods the endpoint serves, besides the `OPTIONS` preflight.
    #[must_use]
    pub fn methods(mut self, methods: &[Method]) -> Self {
        self.methods = methods.to_vec();
        self
    }

    /// Answers browsers from these origins and refuses every other one.
    #[must_use]
    pub fn cors(mut self, cors: Cors) -> Self {
        self.cors = Some(cors);
        self
    }

    /// The time kept back from the invocation deadline to flush the response.
    ///
    /// Negotiation, the policy and the handler run within the deadline minus
    /// this margin; a handler still running when it is reached is a `504`.
    #[must_use]
    pub fn margin(mut self, margin: Duration) -> Self {
        self.margin = margin;
        self
    }

    /// Changes each request before anything reads it: CORS, the method
    /// check, negotiation and the policy all see the prepared request.
    #[must_use]
    pub fn prepare<F>(mut self, prepare: F) -> Self
    where
        F: Fn(&mut lambda_http::Request) + Send + Sync + 'static,
    {
        self.prepare = Some(Box::new(prepare));
        self
    }

    /// Adds headers to every response, success or failure, before CORS.
    #[must_use]
    pub fn finalize<F>(mut self, finalize: F) -> Self
    where
        F: Fn(&Invocation, &mut HeaderMap) + Send + Sync + 'static,
    {
        self.finalize = Some(Box::new(finalize));
        self
    }

    /// The stable operation identifier used for telemetry.
    pub fn operation(&self) -> &str {
        self.operation
    }

    /// Executes one invocation and always yields a response: failures are
    /// rendered. [`StreamApi::run`] calls it for every request; a test calls
    /// it directly.
    pub async fn handle<App, H, F>(
        &self,
        app: Arc<App>,
        event: LambdaEvent<Value>,
        handler: &H,
    ) -> StreamResponse
    where
        H: Fn(Arc<App>, StreamRequest, Context<P::Scope>) -> F,
        F: Future<Output = Result<StreamResponse, Failure>>,
    {
        let (payload, context) = event.into_parts();
        let invocation = crate::runtime::invocation_from(&context);
        #[cfg(feature = "logs")]
        let span = crate::telemetry::logs::invocation_span(self.operation, &invocation);
        let work = self.execute(app, payload, context, &invocation, handler);
        #[cfg(feature = "logs")]
        let work = tracing::Instrument::instrument(work, span.clone());
        let (origin, result) = work.await;
        let mut response = result.unwrap_or_else(|failure| {
            self.log(&failure, &invocation);
            from_response(self.renderer.render(&failure))
        });
        let headers = &mut response.metadata_prelude.headers;
        headers.insert(header::VARY, HeaderValue::from_static("Origin, Accept"));
        if let Some(finalize) = &self.finalize {
            finalize(&invocation, headers);
        }
        if let (Some(cors), Some(origin)) = (&self.cors, origin.as_deref()) {
            if cors.allows(origin) {
                cors.apply(origin, &self.allowed_methods(), headers);
            }
        }
        #[cfg(feature = "otel")]
        crate::telemetry::record_status(&span, response.metadata_prelude.status_code.as_u16());
        response
    }

    /// The pipeline proper: the request's origin, and the response or the
    /// one failure that stopped it.
    ///
    /// The handler's future is boxed, so its layout (often deep in SDK calls)
    /// stays out of the pipeline's own future.
    async fn execute<App, H, F>(
        &self,
        app: Arc<App>,
        payload: Value,
        context: lambda_runtime::Context,
        invocation: &Invocation,
        handler: &H,
    ) -> (Option<String>, Result<StreamResponse, Failure>)
    where
        H: Fn(Arc<App>, StreamRequest, Context<P::Scope>) -> F,
        F: Future<Output = Result<StreamResponse, Failure>>,
    {
        let Ok(native) = lambda_http::request::from_str(&payload.to_string()) else {
            let failure = Failure::new(
                StatusCode::BAD_REQUEST,
                codes::MALFORMED_REQUEST,
                "Invalid HTTP request",
            )
            .with_kind(FailureKind::Decode);
            return (None, Err(failure));
        };
        let mut native = native.with_lambda_context(context);
        if let Some(prepare) = &self.prepare {
            prepare(&mut native);
        }
        let origin = Request::new(&native).header("origin").map(str::to_owned);
        if let (Some(cors), Some(origin)) = (&self.cors, origin.as_deref()) {
            if !cors.allows(origin) {
                let failure = Failure::new(
                    StatusCode::FORBIDDEN,
                    codes::ORIGIN_NOT_ALLOWED,
                    "Origin not allowed",
                )
                .with_kind(FailureKind::Policy);
                return (Some(origin.to_owned()), Err(failure));
            }
        }
        if native.method() == Method::OPTIONS {
            return (origin, Ok(json(StatusCode::NO_CONTENT, &Value::Null)));
        }
        if !self.methods.contains(native.method()) {
            let names: Vec<&str> = self.methods.iter().map(Method::as_str).collect();
            let failure = Failure::new(
                StatusCode::METHOD_NOT_ALLOWED,
                codes::METHOD_NOT_ALLOWED,
                format!("Use {}", names.join(" or ")),
            )
            .with_kind(FailureKind::Decode)
            .with_header("allow", &self.allowed_methods());
            return (origin, Err(failure));
        }
        let deadline = invocation.deadline.with_margin(self.margin);
        let result = deadline
            .run(async {
                let representation = negotiate(Request::new(&native).header("accept"))?;
                let scope = self
                    .policy
                    .authorize(&Request::new(&native), invocation)
                    .await
                    .map_err(|failure| failure.with_kind(FailureKind::Policy))?;
                let request = StreamRequest {
                    native,
                    event: payload,
                    representation,
                };
                Box::pin(handler(
                    app,
                    request,
                    Context::new(invocation.clone(), scope),
                ))
                .await
            })
            .await
            .map_err(super::api::timeout_failure)
            .and_then(std::convert::identity);
        (origin, result)
    }

    /// `GET, OPTIONS`: the methods a preflight and a `405` announce.
    fn allowed_methods(&self) -> String {
        let mut names: Vec<&str> = self.methods.iter().map(Method::as_str).collect();
        names.push("OPTIONS");
        names.join(", ")
    }

    /// Logs a failure's safe metadata once: an error for a 5xx, a debug line
    /// for a refusal.
    fn log(&self, failure: &Failure, invocation: &Invocation) {
        #[cfg(feature = "logs")]
        {
            if failure.is_server_error() {
                tracing::error!(
                    operation = self.operation,
                    request_id = %invocation.request_id,
                    code = failure.code(),
                    kind = ?failure.kind(),
                    "request failed"
                );
            } else {
                tracing::debug!(operation = self.operation, request_id = %invocation.request_id, code = failure.code(), status = failure.status().as_u16(), "request refused");
            }
        }
        #[cfg(not(feature = "logs"))]
        {
            let _ = (failure, invocation);
        }
    }

    /// Runs the Lambda loop for this endpoint.
    ///
    /// Each invocation's future is boxed, one allocation per request: nested
    /// inside the runtime loop's future, the pipeline and a deep handler would
    /// otherwise exceed rustc's query depth limit in a release build.
    ///
    /// # Errors
    ///
    /// Returns a [`RuntimeError`] only when the runtime itself fails; request
    /// failures become responses.
    pub async fn run<App, H, F>(self, app: Arc<App>, handler: H) -> Result<(), RuntimeError>
    where
        App: Send + Sync + 'static,
        H: Fn(Arc<App>, StreamRequest, Context<P::Scope>) -> F + Send + Sync,
        F: Future<Output = Result<StreamResponse, Failure>> + Send,
        P::Scope: Send,
    {
        let api = &self;
        let handler = &handler;
        let service = lambda_runtime::service_fn(move |event: LambdaEvent<Value>| {
            let app = Arc::clone(&app);
            async move {
                Ok::<_, lambda_runtime::Error>(Box::pin(api.handle(app, event, handler)).await)
            }
        });
        lambda_runtime::Runtime::new(service)
            .run()
            .await
            .map_err(crate::runtime::loop_failure)
    }
}
