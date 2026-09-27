//! The buffered pipeline: one route, one ordered set of steps.
//!
//! Every invocation runs, in this order and under one deadline:
//!
//! 1. **admission** — async, before anything is parsed (rate limits, quotas);
//! 2. **decode** — synchronous and bounded, producing an owned input;
//! 3. **policy** — async, producing the scope the handler runs with;
//! 4. **handler** — async, producing a value that implements [`IntoResponse`];
//! 5. **serialization** — that value becomes a response, or a failure.
//!
//! A failure from any step, including the serialization of a success, reaches
//! the same [`ErrorRenderer`], and the finalizer sees every response.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use lambda_http::http::{HeaderName, HeaderValue, StatusCode};
use lambda_http::{service_fn, Adapter, RequestExt};

use super::codes;
use super::failure::{Failure, FailureKind};
use super::policy::{Admission, AdmitAll, Policy};
use super::render::ErrorRenderer;
use super::request::{Request, DEFAULT_BODY_LIMIT};
use super::response::{HttpResponse, IntoResponse};
use crate::{Context, Deadline, Invocation, RuntimeError};

/// Time kept back from Lambda's own timeout, so a rendered `504` still
/// reaches the client.
const RESERVE: Duration = Duration::from_millis(100);

/// Runs on every response, success or failure, just before it is returned.
///
/// Boxed so [`Api`] keeps three generics; one dynamic call per request is
/// nothing beside an HTTP round trip.
pub type Finalizer = Box<dyn Fn(&Invocation, &mut HttpResponse) + Send + Sync>;

/// One HTTP endpoint: its operation name, policy, error renderer and
/// admission check.
///
/// The generics describe what the endpoint is, not builder state. Build it
/// once at startup, then [`Api::run`] it in a Lambda or [`Api::handle`] one
/// request in a test.
///
/// # Examples
///
/// ```
/// use davidrs::http::{Api, PlainErrors, Public};
///
/// let api = Api::new("get-order", Public, PlainErrors).body_limit(64 * 1024);
/// assert_eq!(api.operation(), "get-order");
/// ```
pub struct Api<P, R, A = AdmitAll> {
    operation: &'static str,
    policy: P,
    renderer: R,
    admission: A,
    body_limit: usize,
    finalize: Option<Finalizer>,
}

impl<P, R, A> std::fmt::Debug for Api<P, R, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("operation", &self.operation)
            .field("body_limit", &self.body_limit)
            .field("finalize", &self.finalize.is_some())
            .finish_non_exhaustive()
    }
}

impl<P, R> Api<P, R, AdmitAll>
where
    P: Policy,
    R: ErrorRenderer,
{
    /// Configures an endpoint that admits every request and caps bodies at
    /// [`DEFAULT_BODY_LIMIT`]. Performs no I/O.
    #[must_use]
    pub fn new(operation: &'static str, policy: P, renderer: R) -> Self {
        Self {
            operation,
            policy,
            renderer,
            admission: AdmitAll,
            body_limit: DEFAULT_BODY_LIMIT,
            finalize: None,
        }
    }
}

impl<P, R, A> Api<P, R, A>
where
    P: Policy,
    R: ErrorRenderer,
    A: Admission,
{
    /// Replaces the admission check that runs before the body is read.
    #[must_use]
    pub fn admission<B: Admission>(self, admission: B) -> Api<P, R, B> {
        Api {
            operation: self.operation,
            policy: self.policy,
            renderer: self.renderer,
            admission,
            body_limit: self.body_limit,
            finalize: self.finalize,
        }
    }

    /// Runs `finalize` on every response, whether the handler succeeded or a
    /// failure was rendered.
    ///
    /// Use it for the headers every response carries (cache control, a
    /// request id) and for recording the status. Without it that work would
    /// be duplicated in [`IntoResponse`] and [`ErrorRenderer`], neither of
    /// which sees the invocation.
    #[must_use]
    pub fn finalize<F>(mut self, finalize: F) -> Self
    where
        F: Fn(&Invocation, &mut HttpResponse) + Send + Sync + 'static,
    {
        self.finalize = Some(Box::new(finalize));
        self
    }

    /// Caps the body this endpoint accepts, in bytes.
    ///
    /// The cap applies before the decoder runs, whatever the decoder reads.
    #[must_use]
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.body_limit = bytes;
        self
    }

    /// The stable operation name used in logs and spans.
    pub fn operation(&self) -> &str {
        self.operation
    }

    /// Runs the pipeline for one request and returns the response.
    ///
    /// Failures are rendered, so this always yields a response. [`Api::run`]
    /// calls it for every invocation; tests call it directly with a
    /// hand-built request. A request without a Lambda context gets an empty
    /// request id and Lambda's maximum budget.
    pub async fn handle<App, In, Out, D, H, F>(
        &self,
        app: Arc<App>,
        request: lambda_http::Request,
        decode: &D,
        handler: &H,
    ) -> HttpResponse
    where
        D: Fn(&Request<'_>) -> Result<In, Failure>,
        H: Fn(Arc<App>, In, Context<P::Scope>) -> F,
        F: Future<Output = Result<Out, Failure>>,
        Out: IntoResponse,
    {
        let invocation = invocation_from(&request);
        #[cfg(feature = "logs")]
        let span = crate::telemetry::logs::invocation_span(self.operation, &invocation);
        let work = self.execute(app, &request, &invocation, decode, handler);
        #[cfg(feature = "logs")]
        let work = tracing::Instrument::instrument(work, span.clone());
        let mut response = match work.await {
            Ok(response) => response,
            Err(failure) => {
                self.log(&failure, &invocation);
                self.renderer.render(&failure)
            }
        };
        if let Some(finalize) = &self.finalize {
            finalize(&invocation, &mut response);
        }
        #[cfg(feature = "otel")]
        crate::telemetry::record_status(&span, response.status().as_u16());
        response
    }

    /// The steps of [`Api::handle`], returning the success response or the
    /// one failure that stopped them, so rendering happens in one place.
    ///
    /// Admission gets the whole budget minus [`RESERVE`]; decode, policy,
    /// handler and serialization share what is left. Every decoder, including
    /// one that reads raw bytes, is bounded by the body limit. Serialization
    /// is synchronous and cannot be preempted, so response values should be
    /// bounded. Headers from admission reach the response on both paths.
    async fn execute<App, In, Out, D, H, F>(
        &self,
        app: Arc<App>,
        request: &lambda_http::Request,
        invocation: &Invocation,
        decode: &D,
        handler: &H,
    ) -> Result<HttpResponse, Failure>
    where
        D: Fn(&Request<'_>) -> Result<In, Failure>,
        H: Fn(Arc<App>, In, Context<P::Scope>) -> F,
        F: Future<Output = Result<Out, Failure>>,
        Out: IntoResponse,
    {
        let view = Request::new(request).with_body_limit(self.body_limit);
        let deadline = invocation.deadline.with_margin(RESERVE);
        let admission_headers = deadline
            .run(self.admission.check(&view, invocation))
            .await
            .map_err(timeout_failure)?
            .map_err(|failure| failure.with_kind(FailureKind::Admission))?;

        let result = deadline
            .run(async {
                view.check_body_limit()?;
                let input = decode(&view)?;
                let scope = self
                    .policy
                    .authorize(&view, invocation)
                    .await
                    .map_err(|failure| failure.with_kind(FailureKind::Policy))?;
                let output = handler(app, input, Context::new(invocation.clone(), scope)).await?;
                output.into_response()
            })
            .await
            .map_err(timeout_failure)
            .and_then(std::convert::identity);
        let mut response =
            result.map_err(|failure| failure.with_headers(admission_headers.clone()))?;
        for (name, value) in &admission_headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::try_from(name.as_str()),
                HeaderValue::try_from(value.as_str()),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        Ok(response)
    }

    /// Logs a failure's safe metadata once: operation, request id, code and
    /// kind, never its message or detail.
    fn log(&self, failure: &Failure, invocation: &Invocation) {
        #[cfg(feature = "logs")]
        {
            let (code, kind) = (failure.code(), failure.kind());
            let status = failure.status().as_u16();
            if failure.is_server_error() {
                tracing::error!(
                    operation = self.operation,
                    request_id = %invocation.request_id,
                    code,
                    kind = ?kind,
                    "request failed"
                );
            } else {
                tracing::debug!(
                    operation = self.operation,
                    request_id = %invocation.request_id,
                    code,
                    status,
                    "request refused"
                );
            }
        }
        #[cfg(not(feature = "logs"))]
        {
            let _ = (failure, invocation);
        }
    }

    /// Serves this endpoint from the Lambda loop until the runtime stops.
    ///
    /// # Errors
    ///
    /// Returns a [`RuntimeError`] only when the runtime itself fails; request
    /// failures become responses.
    pub async fn run<App, In, Out, D, H, F>(
        self,
        app: Arc<App>,
        decode: D,
        handler: H,
    ) -> Result<(), RuntimeError>
    where
        App: Send + Sync + 'static,
        In: Send,
        D: Fn(&Request<'_>) -> Result<In, Failure> + Send + Sync,
        H: Fn(Arc<App>, In, Context<P::Scope>) -> F + Send + Sync,
        F: Future<Output = Result<Out, Failure>> + Send,
        Out: IntoResponse,
        P::Scope: Send,
    {
        let api = &self;
        let decode = &decode;
        let handler = &handler;
        let service = service_fn(move |request: lambda_http::Request| {
            let app = Arc::clone(&app);
            async move {
                Ok::<_, std::convert::Infallible>(api.handle(app, request, decode, handler).await)
            }
        });
        lambda_runtime::Runtime::new(Adapter::from(service))
            .run()
            .await
            .map_err(crate::runtime::loop_failure)
    }
}

/// The `504` every HTTP pipeline renders when the invocation budget runs out.
pub(super) fn timeout_failure(_error: RuntimeError) -> Failure {
    Failure::new(
        StatusCode::GATEWAY_TIMEOUT,
        codes::TIMEOUT,
        "Request deadline exceeded",
    )
    .with_kind(FailureKind::Deadline)
}

/// The invocation a request carries, or a stand-in for a request built by
/// hand.
///
/// `lambda_context()` panics when the context is absent, so this uses the
/// checked accessor: a test or a direct call gets an empty request id and
/// Lambda's maximum budget instead of a crash.
fn invocation_from(request: &lambda_http::Request) -> Invocation {
    request.lambda_context_ref().map_or_else(
        || {
            Invocation::new(
                String::new(),
                Deadline::after(crate::runtime::MAX_FUNCTION_TIMEOUT),
            )
        },
        crate::runtime::invocation_from,
    )
}
