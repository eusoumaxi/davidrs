//! A local HTTP/1.1 server that records every request and answers through a
//! closure.
//!
//! Tests use it as whatever remote end the code under test talks to: the
//! Lambda Runtime API, a JWKS endpoint, an upstream service. It binds
//! `127.0.0.1` on an ephemeral port, so tests never touch the network and
//! can run in parallel.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// The response type every responder returns.
pub type Reply = Response<Full<Bytes>>;

type Responder = dyn Fn(Recorded) -> Pin<Box<dyn Future<Output = Reply> + Send>> + Send + Sync;

/// One request as the server received it, with its body fully read.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The method, e.g. `"GET"`.
    pub method: String,
    /// The path and query, e.g. `"/jwks?x=1"`.
    pub path: String,
    /// The request headers.
    pub headers: HeaderMap,
    /// The whole body, chunked or not.
    pub body: Bytes,
}

impl Recorded {
    /// The body as UTF-8 text, lossily.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// One header as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// A running server. Dropping it stops accepting connections.
pub struct Server {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    accept: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Starts a server whose answer to each request is `respond(request)`.
    ///
    /// The responder is async so it can hold a request open, the way the
    /// Runtime API holds `/next` until an event is available.
    pub async fn start<F, Fut>(respond: F) -> Self
    where
        F: Fn(Recorded) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Reply> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let respond: Arc<Responder> = Arc::new(move |request| Box::pin(respond(request)));
        let log = Arc::clone(&requests);
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let respond = Arc::clone(&respond);
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let respond = Arc::clone(&respond);
                        let log = Arc::clone(&log);
                        async move {
                            let recorded = record(request).await;
                            log.lock().expect("log").push(recorded.clone());
                            Ok::<_, Infallible>(respond(recorded).await)
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            addr,
            requests,
            accept,
        }
    }

    /// A server that answers every request with the same status, media type
    /// and body.
    pub async fn fixed(status: u16, content_type: &'static str, body: impl Into<Bytes>) -> Self {
        let body: Bytes = body.into();
        Self::start(move |_| {
            let body = body.clone();
            async move { reply(status, content_type, body) }
        })
        .await
    }

    /// `http://127.0.0.1:<port><path>`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// `127.0.0.1:<port>`, for clients configured with a host rather than a URL.
    pub fn authority(&self) -> String {
        self.addr.to_string()
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("log").clone()
    }

    /// How many requests reached `path` (query excluded).
    pub fn hits(&self, path: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.path.split('?').next() == Some(path))
            .count()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// A response with a status, a media type and a body.
pub fn reply(status: u16, content_type: &str, body: impl Into<Bytes>) -> Reply {
    Response::builder()
        .status(StatusCode::from_u16(status).expect("status"))
        .header("content-type", content_type)
        .body(Full::new(body.into()))
        .expect("response")
}

/// A `200` JSON response.
pub fn json(value: &serde_json::Value) -> Reply {
    reply(200, "application/json", value.to_string())
}

async fn record(request: Request<Incoming>) -> Recorded {
    let (parts, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    Recorded {
        method: parts.method.to_string(),
        path: parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string),
        headers: parts.headers,
        body,
    }
}
