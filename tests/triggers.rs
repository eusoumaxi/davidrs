//! What the trigger adapters read from an invocation: the platform context
//! through `invocation_from`, and the EventBridge envelope as `Event<T>`.
//!
//! The loops themselves run in `tests/lambda_loop.rs`.
#![cfg(feature = "runtime")]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use davidrs::runtime::invocation_from;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_millis() as u64
}

fn context(deadline: u64) -> lambda_runtime::Context {
    let mut context = lambda_runtime::Context::default();
    context.request_id = "r-1".to_owned();
    context.deadline = deadline;
    context
}

fn remaining(deadline: u64) -> Duration {
    invocation_from(&context(deadline)).deadline.remaining()
}

#[test]
fn an_epoch_deadline_becomes_the_same_budget_from_now() {
    let left = remaining(now_ms() + 30_000);
    assert!(
        left > Duration::from_secs(25) && left <= Duration::from_secs(30),
        "{left:?}"
    );
}

/// A local emulator sends the budget itself, e.g. `600000`; read as an epoch
/// it would be January 1970 and every invocation would start expired.
#[test]
fn a_relative_deadline_of_up_to_fifteen_minutes_is_a_budget_from_now() {
    for budget in [Duration::from_secs(600), Duration::from_secs(15 * 60)] {
        let left = remaining(budget.as_millis() as u64);
        assert!(
            left > budget - Duration::from_secs(5) && left <= budget,
            "{left:?}"
        );
    }
}

#[test]
fn an_epoch_already_past_is_an_expired_deadline() {
    assert!(invocation_from(&context(now_ms() - 1_000))
        .deadline
        .is_expired());
    assert!(invocation_from(&context(15 * 60 * 1_000 + 1))
        .deadline
        .is_expired());
}

#[test]
fn the_request_id_trace_header_and_arn_reach_the_invocation() {
    let arn = "arn:aws:lambda:us-east-1:123456789012:function:example";
    let mut native = context(now_ms() + 30_000);
    native.xray_trace_id = Some("Root=1-abc;Parent=def;Sampled=1".to_owned());
    native.invoked_function_arn = arn.to_owned();
    native.tenant_id = Some("tenant-7".to_owned());
    let invocation = invocation_from(&native);
    assert_eq!(invocation.request_id, "r-1");
    assert_eq!(invocation.trace_root(), Some("1-abc"));
    assert_eq!(invocation.invoked_arn.as_deref(), Some(arn));
    assert_eq!(invocation.tenant_id.as_deref(), Some("tenant-7"));
}

#[test]
fn an_empty_arn_or_tenant_is_none() {
    let mut native = context(now_ms() + 30_000);
    native.tenant_id = Some(String::new());
    let invocation = invocation_from(&native);
    assert_eq!(invocation.invoked_arn, None);
    assert_eq!(invocation.tenant_id, None);
}

#[cfg(feature = "event")]
mod event {
    use davidrs::event::Event;
    use serde_json::json;

    #[derive(Debug, serde::Deserialize)]
    struct OrderCreated {
        order_id: String,
    }

    #[test]
    fn an_eventbridge_envelope_deserializes_with_a_typed_detail() {
        let event: Event<OrderCreated> = serde_json::from_value(json!({
            "version": "0",
            "id": "6a7e8feb-b491-4cf7-a9f1-bf3703467718",
            "detail-type": "order.created",
            "source": "shop.orders",
            "account": "123456789012",
            "time": "2026-01-01T00:00:00Z",
            "region": "us-east-1",
            "resources": [],
            "detail": { "order_id": "o-1" }
        }))
        .expect("envelope");
        assert_eq!(event.id, "6a7e8feb-b491-4cf7-a9f1-bf3703467718");
        assert_eq!(event.source, "shop.orders");
        assert_eq!(event.detail_type, "order.created");
        assert_eq!(event.time.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(event.detail.order_id, "o-1");
    }

    #[test]
    fn an_envelope_without_a_time_still_deserializes() {
        let event: Event<serde_json::Value> = serde_json::from_value(json!({
            "id": "e-1",
            "detail-type": "order.created",
            "source": "shop.orders",
            "detail": {}
        }))
        .expect("envelope");
        assert_eq!(event.time, None);
    }
}
