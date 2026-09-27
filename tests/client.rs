//! The outbound client and its bounded readers, against a local upstream.
//!
//! What matters is what the client refuses to do: wait forever, follow a
//! redirect, buffer an unbounded body, or print a URL that may carry a key.
#![cfg(feature = "client")]

mod support;

use std::error::Error as _;
use std::time::{Duration, Instant};

use davidrs::client::{self, json_bounded, read_bounded, send_error, Limits};
use davidrs::RuntimeError;
use support::server::{reply, Server};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// A credential an upstream takes in its query string.
const SECRET: &str = "sk-query-secret-1234";

fn http() -> reqwest::Client {
    client::build(Limits::default()).expect("client")
}

async fn get(server: &Server, path: &str) -> reqwest::Response {
    http().get(server.url(path)).send().await.expect("send")
}

/// An upstream that answers `body` with `Transfer-Encoding: chunked` and no
/// `Content-Length`, so only the chunk-by-chunk check can stop it.
async fn chunked(body: &'static str) -> Server {
    Server::start(move |_| async move {
        let mut response = reply(200, "text/plain", body);
        response
            .headers_mut()
            .insert("transfer-encoding", "chunked".parse().expect("header"));
        response
    })
    .await
}

fn is_limit(error: &RuntimeError, expected: usize) -> bool {
    matches!(error, RuntimeError::LimitExceeded { kind: "response bytes", limit } if *limit == expected as u64)
}

#[test]
fn the_default_limits_are_two_seconds_to_connect_and_ten_for_the_request() {
    let limits = Limits::default();
    assert_eq!(limits.connect, Duration::from_secs(2));
    assert_eq!(limits.request, Duration::from_secs(10));
    assert_eq!(
        limits,
        Limits::new(Duration::from_secs(2), Duration::from_secs(10))
    );
}

#[test]
fn a_client_can_be_built_more_than_once() {
    client::build(Limits::default()).expect("first");
    client::build(Limits::default()).expect("second");
}

#[tokio::test]
async fn a_response_slower_than_the_request_limit_times_out() {
    let server = Server::start(|_| async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        reply(200, "text/plain", "late")
    })
    .await;
    let http = client::build(Limits::new(
        Duration::from_secs(1),
        Duration::from_millis(200),
    ))
    .expect("client");
    let started = Instant::now();
    let error = http
        .get(server.url("/slow"))
        .send()
        .await
        .expect_err("times out");
    assert!(error.is_timeout());
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn a_redirect_is_returned_not_followed() {
    let server = Server::start(|request| async move {
        if request.path == "/start" {
            let mut response = reply(302, "text/plain", "");
            response
                .headers_mut()
                .insert("location", "/elsewhere".parse().expect("header"));
            response
        } else {
            reply(200, "text/plain", "followed")
        }
    })
    .await;
    let response = get(&server, "/start").await;
    assert_eq!(response.status(), 302);
    assert_eq!(server.hits("/elsewhere"), 0);
}

#[tokio::test]
async fn a_body_exactly_at_the_limit_is_read_whole() {
    let server = Server::fixed(200, "text/plain", "0123456789").await;
    let body = read_bounded(get(&server, "/").await, 10)
        .await
        .expect("at the cap");
    assert_eq!(body, "0123456789");
}

#[tokio::test]
async fn a_declared_length_over_the_limit_fails_before_the_body_is_read() {
    let server = Server::fixed(200, "text/plain", "0123456789").await;
    let error = read_bounded(get(&server, "/").await, 9)
        .await
        .expect_err("over the cap");
    assert!(is_limit(&error, 9), "{error}");
}

#[tokio::test]
async fn a_chunked_body_is_read_whole_up_to_the_limit() {
    let server = chunked("0123456789").await;
    let response = get(&server, "/").await;
    assert_eq!(response.content_length(), None);
    let body = read_bounded(response, 10).await.expect("at the cap");
    assert_eq!(body, "0123456789");
}

#[tokio::test]
async fn a_chunked_body_over_the_limit_fails_while_reading() {
    let server = chunked("0123456789").await;
    let error = read_bounded(get(&server, "/").await, 9)
        .await
        .expect_err("over the cap");
    assert!(is_limit(&error, 9), "{error}");
}

/// An upstream that promises 100 bytes, sends 5 and closes the connection.
///
/// It is a raw socket because a well-behaved server, like the one in
/// `support`, refuses to send a body shorter than its `Content-Length`.
async fn cut_short() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort")
            .await;
    });
    format!("http://{address}/cut?key={SECRET}")
}

#[tokio::test]
async fn a_body_cut_short_is_an_error_without_the_url() {
    let response = http()
        .get(cut_short().await)
        .send()
        .await
        .expect("headers arrive");
    let error = read_bounded(response, 1024)
        .await
        .expect_err("the body ends early");
    let text = davidrs::error_chain(&error);
    assert!(text.starts_with("reading the response body"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
}

#[tokio::test]
async fn send_error_drops_the_url_that_reqwest_would_print() {
    let url = format!("http://127.0.0.1:1/unreachable?key={SECRET}");
    let error = http().get(url).send().await.expect_err("refused");
    assert!(error.to_string().contains(SECRET), "reqwest prints the URL");
    let mapped = send_error("calling the upstream", error);
    let text = davidrs::error_chain(&mapped);
    assert!(text.starts_with("calling the upstream"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(mapped.source().is_some(), "the cause stays reachable");
}

#[tokio::test]
async fn json_bounded_decodes_a_json_body() {
    let server = Server::fixed(200, "application/json", r#"{"id":"order-1"}"#).await;
    let value: serde_json::Value = json_bounded(get(&server, "/").await, 1024)
        .await
        .expect("json");
    assert_eq!(value["id"], "order-1");
}

#[tokio::test]
async fn json_bounded_refuses_a_body_that_is_not_json() {
    let server = Server::fixed(200, "application/json", "not json").await;
    let error = json_bounded::<serde_json::Value>(get(&server, "/").await, 1024)
        .await
        .expect_err("not json");
    assert!(
        error.to_string().starts_with("decoding the response body"),
        "{error}"
    );
}

#[tokio::test]
async fn json_bounded_applies_the_byte_limit_before_decoding() {
    let server = Server::fixed(200, "application/json", r#"{"id":"order-1"}"#).await;
    let error = json_bounded::<serde_json::Value>(get(&server, "/").await, 4)
        .await
        .expect_err("over the cap");
    assert!(is_limit(&error, 4), "{error}");
}

/// The readers never look at the status: what a `500` means is up to the
/// caller, and its body is often the only diagnostic there is.
#[tokio::test]
async fn an_error_status_is_read_like_any_other_body() {
    let server = Server::fixed(500, "application/json", r#"{"error":"busy"}"#).await;
    let response = get(&server, "/").await;
    assert_eq!(response.status(), 500);
    let value: serde_json::Value = json_bounded(response, 1024).await.expect("json");
    assert_eq!(value["error"], "busy");
}
