//! The synthetic invocations and requests `test_support` builds.
#![cfg(feature = "test-support")]

use std::time::Duration;

use davidrs::test_support::{expired_invocation, invocation, invocation_with_budget};

#[test]
fn an_invocation_has_thirty_seconds_left() {
    let invocation = invocation("r-1");
    assert_eq!(invocation.request_id, "r-1");
    let left = invocation.deadline.remaining();
    assert!(
        left > Duration::from_secs(25) && left <= Duration::from_secs(30),
        "{left:?}"
    );
}

#[test]
fn an_expired_invocation_has_no_budget_left() {
    let invocation = expired_invocation("r-1");
    assert_eq!(invocation.request_id, "r-1");
    assert!(invocation.deadline.expired());
}

#[test]
fn an_invocation_with_a_budget_has_that_budget_left() {
    let invocation = invocation_with_budget("r-1", Duration::from_millis(500));
    let left = invocation.deadline.remaining();
    assert!(
        left > Duration::ZERO && left <= Duration::from_millis(500),
        "{left:?}"
    );
}

#[cfg(feature = "http")]
mod http {
    use std::time::{SystemTime, UNIX_EPOCH};

    use davidrs::test_support::{get, post_json, with_context};
    use lambda_http::{Body, Request, RequestExt as _};

    /// The request id and the milliseconds left in the attached context.
    fn context_of(request: &Request) -> (String, u64) {
        let context = request.lambda_context_ref().expect("a Lambda context");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_millis() as u64;
        (
            context.request_id.clone(),
            context.deadline.saturating_sub(now),
        )
    }

    #[test]
    fn with_context_attaches_a_request_id_and_thirty_seconds() {
        let (request_id, left_ms) = context_of(&with_context(Request::new(Body::Empty)));
        assert_eq!(request_id, "test-request-id");
        assert!((25_000..=30_000).contains(&left_ms), "{left_ms} ms");
    }

    #[test]
    fn get_builds_an_empty_get_with_a_lambda_context() {
        let request = get("/items?limit=2");
        assert_eq!(request.method(), "GET");
        assert_eq!(request.uri(), "/items?limit=2");
        assert!(matches!(request.body(), Body::Empty));
        assert_eq!(context_of(&request).0, "test-request-id");
    }

    #[test]
    fn post_json_builds_a_json_post_with_a_lambda_context() {
        let request = post_json("/items", r#"{"name":"widget"}"#);
        assert_eq!(request.method(), "POST");
        assert_eq!(request.uri(), "/items");
        assert_eq!(request.headers()["content-type"], "application/json");
        assert!(matches!(request.body(), Body::Text(text) if text == r#"{"name":"widget"}"#));
        assert_eq!(context_of(&request).0, "test-request-id");
    }
}
