//! The value types of the empty crate: [`Invocation`], [`Deadline`],
//! [`Context`], [`RuntimeError`] and [`davidrs::error_chain`].
//!
//! The property that matters most is that a budget only ever shrinks: a child
//! cannot outlive its parent, a cleanup margin cannot move a deadline later,
//! and a retry inside an invocation does not get a fresh allowance.

use std::time::{Duration, Instant};

use davidrs::{error_chain, Context, Deadline, Invocation, RuntimeError};

fn budget(duration: Duration) -> Deadline {
    Deadline::in_from_now(duration)
}

#[test]
fn trace_root_is_the_root_id_of_the_trace_header() {
    let traced = |header: Option<&str>| {
        Invocation::new("r", budget(Duration::from_secs(1)))
            .with_trace_id(header.map(str::to_owned))
            .trace_root()
            .map(str::to_owned)
    };
    assert_eq!(
        traced(Some("Root=1-abc;Parent=def;Sampled=1")),
        Some("1-abc".to_owned())
    );
    assert_eq!(
        traced(Some("Parent=def; Root=1-abc")),
        Some("1-abc".to_owned())
    );
    assert_eq!(traced(Some("Parent=def;Sampled=1")), None);
    assert_eq!(traced(None), None);
}

#[test]
fn a_new_invocation_has_no_trace_or_arn_until_they_are_set() {
    let deadline = budget(Duration::from_secs(1));
    let invocation = Invocation::new("r-1", deadline);
    assert_eq!(invocation.request_id, "r-1");
    assert_eq!(invocation.deadline, deadline);
    assert_eq!(invocation.trace_id, None);
    assert_eq!(invocation.invoked_arn, None);
    assert_eq!(invocation.tenant_id, None);

    let arn = "arn:aws:lambda:us-east-1:123456789012:function:example";
    let invocation = invocation
        .with_invoked_arn(Some(arn.to_owned()))
        .with_tenant_id(Some("tenant-7".to_owned()));
    assert_eq!(invocation.invoked_arn.as_deref(), Some(arn));
    assert_eq!(invocation.tenant_id.as_deref(), Some("tenant-7"));
}

#[test]
fn elapsed_grows_from_when_the_invocation_arrived() {
    let invocation = Invocation::new("r", budget(Duration::from_secs(30)));
    let first = invocation.elapsed();
    std::thread::sleep(Duration::from_millis(5));
    assert!(invocation.elapsed() >= first + Duration::from_millis(5));
}

#[test]
fn a_deadline_counts_down_to_its_instant() {
    let at = Instant::now() + Duration::from_secs(10);
    let deadline = Deadline::at(at);
    assert_eq!(deadline.instant(), at);
    assert!(deadline.remaining() <= Duration::from_secs(10));
    assert!(deadline.remaining() > Duration::from_secs(5));
    assert!(!deadline.expired());
}

#[test]
fn an_expired_deadline_reports_zero_and_its_children_are_expired() {
    let past = Deadline::at(Instant::now() - Duration::from_secs(1));
    assert!(past.expired());
    assert_eq!(past.remaining(), Duration::ZERO);
    assert!(past.child(Duration::from_secs(60)).expired());
}

/// Each retry derives its budget from the same parent, so the parent alone
/// decides when work must stop.
#[test]
fn a_child_budget_is_clamped_to_its_parent() {
    let parent = budget(Duration::from_millis(100));
    assert_eq!(parent.child(Duration::from_secs(300)), parent);
    assert!(parent.child(Duration::from_millis(10)) < parent);
}

#[test]
fn a_margin_moves_a_deadline_earlier_and_never_later() {
    let deadline = budget(Duration::from_secs(30));
    assert_eq!(
        deadline.with_margin(Duration::from_secs(5)).instant() + Duration::from_secs(5),
        deadline.instant()
    );
    assert_eq!(deadline.with_margin(Duration::MAX), deadline);
}

#[test]
fn earliest_picks_the_tighter_budget() {
    let soon = budget(Duration::from_millis(10));
    let later = budget(Duration::from_secs(10));
    assert_eq!(soon.earliest(later), soon);
    assert_eq!(later.earliest(soon), soon);
}

#[test]
fn a_context_carries_its_invocation_and_scope() {
    let deadline = budget(Duration::from_secs(1));
    let context = Context::new(Invocation::new("r-1", deadline), "user-7".to_owned());
    assert_eq!(context.scope(), "user-7");
    assert_eq!(context.invocation().request_id, "r-1");
    assert_eq!(context.deadline(), deadline);
    assert_eq!(context.into_scope(), "user-7");
}

#[cfg(feature = "runtime")]
#[tokio::test]
async fn work_that_finishes_in_time_returns_its_value() {
    let outcome = budget(Duration::from_secs(5)).run(async { 7 }).await;
    assert_eq!(outcome.expect("in time"), 7);
}

#[cfg(feature = "runtime")]
#[tokio::test]
async fn work_that_overruns_reports_the_deadline_not_a_wrong_answer() {
    let outcome = budget(Duration::from_millis(30))
        .run(std::future::pending::<()>())
        .await;
    match outcome {
        Err(RuntimeError::DeadlineExceeded { elapsed }) => {
            assert!(elapsed >= Duration::from_millis(20), "{elapsed:?}");
        }
        other => panic!("expected a deadline failure, got {other:?}"),
    }
}

#[cfg(feature = "runtime")]
#[tokio::test]
async fn an_expired_deadline_does_not_start_the_work() {
    let past = Deadline::at(Instant::now() - Duration::from_secs(1));
    let outcome: Result<(), RuntimeError> = past
        .run(async { panic!("work past the deadline must not start") })
        .await;
    assert!(matches!(
        outcome,
        Err(RuntimeError::DeadlineExceeded { elapsed }) if elapsed.is_zero()
    ));
}

#[test]
fn each_variant_says_what_went_wrong() {
    let cases = [
        (
            RuntimeError::Configuration("missing TABLE".to_owned()),
            "configuration: missing TABLE",
        ),
        (
            RuntimeError::DeadlineExceeded {
                elapsed: Duration::from_millis(12),
            },
            "deadline exceeded after 12 ms",
        ),
        (
            RuntimeError::LimitExceeded {
                kind: "decoded bytes",
                limit: 1024,
            },
            "limit exceeded: 1024 of decoded bytes",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
}

#[test]
fn other_keeps_its_source_in_the_chain() {
    let error = RuntimeError::other("saving order", std::io::Error::other("disk full"));
    assert_eq!(error.to_string(), "saving order");
    assert_eq!(error_chain(&error), "saving order: disk full");
}

#[test]
fn a_message_has_no_source_and_converts_from_text() {
    for error in [
        RuntimeError::message("no rows"),
        RuntimeError::from("no rows"),
        RuntimeError::from("no rows".to_owned()),
    ] {
        assert!(std::error::Error::source(&error).is_none());
        assert_eq!(error_chain(&error), "no rows");
    }
}

#[test]
fn error_chain_joins_every_source_in_order() {
    let error = RuntimeError::other(
        "saving order",
        RuntimeError::other("writing line 3", std::io::Error::other("disk full")),
    );
    assert_eq!(
        error_chain(&error),
        "saving order: writing line 3: disk full"
    );
}
