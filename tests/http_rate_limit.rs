//! The rate-limit headers: what a client reads, whatever counts the requests.
#![cfg(feature = "http")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use davidrs::http::{
    codes, Admission as _, Api, Counter, ErrorDefinition, Failure, FailureKind, Json, PlainErrors,
    Public, RateLimit, RateLimitConfig, RateLimited, Request, StatusCode,
};
use davidrs::{Context, Deadline, Invocation, RuntimeError};
use serde_json::{json, Value};

const CONFIG: RateLimitConfig = RateLimitConfig::new("search", 60, Duration::from_secs(300));

/// An outcome with `remaining` requests left and a reset `seconds` from now.
fn outcome(remaining: i64, seconds: u64) -> RateLimit {
    RateLimit::new(
        &CONFIG,
        remaining,
        SystemTime::now() + Duration::from_secs(seconds),
    )
}

#[test]
fn new_takes_the_quota_and_window_from_the_policy() {
    let at = UNIX_EPOCH + Duration::from_secs(1);
    let limit = RateLimit::new(&CONFIG, 7, at);
    assert_eq!(
        (limit.limit, limit.window, limit.remaining, limit.resets_at),
        (60, Duration::from_secs(300), 7, at)
    );
}

#[test]
fn the_headers_are_the_policy_then_the_current_state() {
    assert_eq!(
        outcome(7, 90).headers(),
        [
            (
                "RateLimit-Policy".to_owned(),
                "\"default\";q=60;w=300".to_owned()
            ),
            ("RateLimit".to_owned(), "\"default\";r=7;t=90".to_owned()),
        ]
    );
}

#[test]
fn a_budget_is_exceeded_only_below_zero() {
    assert!(!outcome(0, 30).is_exceeded(), "the last request is allowed");
    assert!(outcome(-1, 30).is_exceeded());
}

#[test]
fn an_exhausted_budget_never_reports_a_negative_count() {
    assert_eq!(outcome(-3, 30).headers()[1].1, "\"default\";r=0;t=30");
}

#[test]
fn a_zero_window_is_left_out_of_the_policy() {
    let config = RateLimitConfig::new("search", 0, Duration::ZERO);
    let limit = RateLimit::new(&config, 0, UNIX_EPOCH);
    assert_eq!(limit.headers()[0].1, "\"default\";q=0");
}

/// A client that waits the whole number of seconds it is told must not come
/// back early.
#[test]
fn the_window_and_the_reset_round_a_partial_second_up() {
    let config = RateLimitConfig::new("search", 60, Duration::from_millis(1_500));
    let limit = RateLimit::new(&config, 1, SystemTime::now() + Duration::from_secs(5));
    assert_eq!(
        limit.headers(),
        [
            (
                "RateLimit-Policy".to_owned(),
                "\"default\";q=60;w=2".to_owned()
            ),
            ("RateLimit".to_owned(), "\"default\";r=1;t=5".to_owned()),
        ]
    );
}

#[test]
fn the_wait_counts_down_to_zero_and_stays_there() {
    assert!(outcome(1, 5).reset_after() > Duration::from_secs(4));
    let reset = RateLimit::new(&CONFIG, 10, SystemTime::now() - Duration::from_secs(5));
    assert_eq!(reset.reset_after(), Duration::ZERO);
}

#[test]
fn extreme_values_render_without_overflowing() {
    let config = RateLimitConfig::new("search", u32::MAX, Duration::MAX);
    let headers = RateLimit::new(&config, i64::MIN, UNIX_EPOCH).headers();
    assert_eq!(
        headers[0].1,
        format!("\"default\";q={};w={}", u32::MAX, u64::MAX)
    );
    assert_eq!(headers[1].1, "\"default\";r=0;t=0");
}

/// A counter that answers with a fixed number of requests left, or fails,
/// and records the keys it counted.
struct Scripted {
    remaining: Option<i64>,
    keys: Arc<Mutex<Vec<String>>>,
}

impl Scripted {
    /// A counter with `remaining` requests left, and the log of its keys.
    fn left(remaining: i64) -> (Self, Arc<Mutex<Vec<String>>>) {
        let keys = Arc::<Mutex<Vec<String>>>::default();
        let counter = Self {
            remaining: Some(remaining),
            keys: Arc::clone(&keys),
        };
        (counter, keys)
    }

    /// A counter whose store cannot be reached.
    fn broken() -> Self {
        Self {
            remaining: None,
            keys: Arc::default(),
        }
    }
}

impl Counter for Scripted {
    async fn hit(&self, key: &str, config: &RateLimitConfig) -> Result<RateLimit, RuntimeError> {
        self.keys.lock().expect("keys").push(key.to_owned());
        let remaining = self
            .remaining
            .ok_or_else(|| RuntimeError::message("store unreachable"))?;
        Ok(RateLimit::new(
            config,
            remaining,
            SystemTime::now() + Duration::from_secs(30),
        ))
    }
}

/// An HTTP API request from `ip`, carrying an `x-api-key`.
fn from(ip: &str) -> lambda_http::Request {
    let event = json!({
        "version": "2.0",
        "rawPath": "/search",
        "rawQueryString": "",
        "headers": { "x-api-key": "key-1" },
        "requestContext": {
            "http": { "method": "GET", "path": "/search", "sourceIp": ip },
            "requestId": "r1", "stage": "$default", "timeEpoch": 0
        },
        "isBase64Encoded": false
    });
    lambda_http::request::from_str(&event.to_string()).expect("event")
}

async fn check<C: Counter>(
    admission: &RateLimited<C>,
    request: &lambda_http::Request,
) -> Result<Vec<(String, String)>, Failure> {
    let invocation = Invocation::new("r1", Deadline::after(Duration::from_secs(5)));
    admission.check(&Request::new(request), &invocation).await
}

#[tokio::test]
async fn an_admitted_request_is_counted_per_caller_address_and_reports_its_budget() {
    let (counter, keys) = Scripted::left(9);
    let admission = RateLimited::new(counter, CONFIG);
    let headers = check(&admission, &from("198.51.100.7"))
        .await
        .expect("admitted");
    assert_eq!(headers[0].0, "RateLimit-Policy");
    assert!(
        headers[1].1.starts_with("\"default\";r=9;t="),
        "{headers:?}"
    );
    assert_eq!(*keys.lock().expect("keys"), ["198.51.100.7"]);
    assert!(format!("{admission:?}").contains("search"));
}

#[tokio::test]
async fn past_the_limit_the_request_is_refused_with_429_retry_after_and_the_budget() {
    let (counter, _) = Scripted::left(-1);
    let failure = check(&RateLimited::new(counter, CONFIG), &from("198.51.100.7"))
        .await
        .expect_err("refused");
    assert_eq!(failure.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(failure.code(), codes::RATE_LIMITED);
    assert_eq!(failure.kind(), FailureKind::Admission);
    let names: Vec<&str> = failure
        .headers()
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(names, ["retry-after", "ratelimit-policy", "ratelimit"]);
    let retry: i64 = failure.headers()[0]
        .1
        .to_str()
        .expect("text")
        .parse()
        .expect("seconds");
    assert!((29..=30).contains(&retry), "{retry}");
}

#[tokio::test]
async fn a_counter_that_cannot_count_refuses_rather_than_admitting_uncounted() {
    let failure = check(
        &RateLimited::new(Scripted::broken(), CONFIG),
        &from("198.51.100.7"),
    )
    .await
    .expect_err("refused");
    assert!(failure.is_server_error());
    assert_eq!(failure.kind(), FailureKind::Admission);
    assert!(
        !failure.public_message().contains("unreachable"),
        "the cause stays internal"
    );
}

#[tokio::test]
async fn the_key_and_the_refusal_are_configurable() {
    let (counter, keys) = Scripted::left(-1);
    let admission = RateLimited::new(counter, CONFIG)
        .key(|request| {
            request
                .header("x-api-key")
                .unwrap_or("anonymous")
                .to_owned()
        })
        .refusal(ErrorDefinition::new(
            "ERROR_QUOTA",
            StatusCode::TOO_MANY_REQUESTS,
            "Quota used up",
        ));
    let failure = check(&admission, &from("198.51.100.7"))
        .await
        .expect_err("refused");
    assert_eq!(
        (failure.code(), failure.public_message()),
        ("ERROR_QUOTA", "Quota used up")
    );
    assert_eq!(*keys.lock().expect("keys"), ["key-1"]);
}

#[tokio::test]
async fn the_pipeline_puts_the_budget_on_a_success_and_never_runs_a_refused_handler() {
    async fn search(
        _app: Arc<()>,
        _input: (),
        _context: Context<()>,
    ) -> Result<Json<Value>, Failure> {
        Ok(Json(json!({ "results": [] })))
    }
    let decode = |_: &Request<'_>| Ok(());
    let (counter, _) = Scripted::left(3);
    let admitted = Api::new("search", Public, PlainErrors)
        .admission(RateLimited::new(counter, CONFIG))
        .handle(Arc::new(()), from("198.51.100.7"), &decode, &search)
        .await;
    assert_eq!(admitted.status(), StatusCode::OK);
    assert!(admitted.headers().contains_key("ratelimit"));
    let (counter, _) = Scripted::left(-1);
    let refused = Api::new("search", Public, PlainErrors)
        .admission(RateLimited::new(counter, CONFIG))
        .handle(Arc::new(()), from("198.51.100.7"), &decode, &search)
        .await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(refused.headers().contains_key("retry-after"));
}
