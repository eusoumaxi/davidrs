# EventBridge publishing

The [`events`](crate::events) module publishes events with `PutEvents` and reports what happened to each one. [`publish`](crate::events::publish) sends one event, [`publish_batch`](crate::events::publish_batch) up to [`MAX_ENTRIES`](crate::events::MAX_ENTRIES) (10) in one call, and both return a [`PublishOutcome`](crate::events::PublishOutcome) with one [`EntryOutcome`](crate::events::EntryOutcome) per input, in input order. Where an event goes is an [`EventRoute`](crate::events::EventRoute): a source and a detail type, written as a `const`.

## Why it exists

`PutEvents` answers `200` while rejecting individual entries. Code that only checks the SDK's `Result` reports success for events that never reached the bus, and nothing downstream ever runs. Here every entry has an explicit outcome:

- [`Accepted`](crate::events::EntryOutcome::Accepted) carries the event id EventBridge assigned.
- [`Rejected`](crate::events::EntryOutcome::Rejected) carries the service's error code and message.
- [`Unknown`](crate::events::EntryOutcome::Unknown) means the call did not complete before the deadline, or the service said nothing about that entry. The event may or may not have been published: publishing it again may duplicate it, and assuming it was lost may be wrong too.

The call also runs under the invocation [`Deadline`](crate::Deadline), so a slow `PutEvents` cannot hold the invocation until Lambda stops it.

## How to use it

Declare each route once, next to the payload type it carries. The payload only needs `Serialize`; it is sent as the event's `detail`.

```rust,no_run
use davidrs::events::{self, EntryOutcome, EventRoute};
use davidrs::{Deadline, RuntimeError};

const ORDER_SHIPPED: EventRoute = EventRoute {
    source: "example.orders",
    detail_type: "order.shipped",
};

/// The detail of `order.shipped`.
#[derive(serde::Serialize)]
struct OrderShipped {
    order_id: String,
    carrier: String,
}

async fn announce(client: &aws_sdk_eventbridge::Client, shipped: &OrderShipped, deadline: Deadline) -> Result<(), RuntimeError> {
    let outcome = events::publish(client, "orders", &ORDER_SHIPPED, shipped, deadline).await?;
    match &outcome.entries[0] {
        EntryOutcome::Accepted(_) => Ok(()),
        EntryOutcome::Rejected { code, .. } => Err(RuntimeError::message(format!("order.shipped rejected: {code}"))),
        _ => Err(RuntimeError::message("order.shipped may not have been published")),
    }
}
```

A rejected entry is not an error: `Err` is reserved for a detail that cannot be serialized and for a call the service refused as a whole. More than [`MAX_ENTRIES`](crate::events::MAX_ENTRIES) details is refused with [`RuntimeError::LimitExceeded`](crate::RuntimeError::LimitExceeded) before anything is sent, so a larger set is split by the caller, which also decides what to do with each outcome:

```rust,no_run
use davidrs::events::{self, EventRoute, MAX_ENTRIES};
use davidrs::{Deadline, RuntimeError};

const LINE_ADDED: EventRoute = EventRoute {
    source: "example.orders",
    detail_type: "line.added",
};

/// Publishes every line and returns the ones to publish again.
async fn publish_lines(
    client: &aws_sdk_eventbridge::Client,
    lines: &[serde_json::Value],
    deadline: Deadline,
) -> Result<Vec<serde_json::Value>, RuntimeError> {
    let mut again = Vec::new();
    for chunk in lines.chunks(MAX_ENTRIES) {
        let outcome = events::publish_batch(client, "orders", &LINE_ADDED, chunk, deadline).await?;
        again.extend(outcome.failed_indexes().into_iter().map(|index| chunk[index].clone()));
    }
    Ok(again)
}
```

[`PublishOutcome::all_accepted`](crate::events::PublishOutcome::all_accepted) and [`failed_indexes`](crate::events::PublishOutcome::failed_indexes) summarize a call; an `Unknown` entry counts as not accepted. An empty slice returns an empty outcome without calling the service.

## Use cases

- Announcing a state change after it is stored, and failing the invocation when the announcement was rejected, so a retry publishes it again.
- An outbox relay that publishes pending events in chunks of ten and keeps the ones that were not accepted.
- Recording the event id EventBridge assigned, to correlate a publication with what its consumers received.

## What it does not do

- **No retries.** Publishing a rejected or unknown entry again is the caller's decision, since only the caller knows whether a duplicate is acceptable. Consumers of EventBridge must be idempotent anyway: delivery is at least once.
- **No splitting.** A call carries at most ten entries; split larger sets as above.
- **No size check.** `PutEvents` limits the total size of a request, and the service refuses an oversized one as a whole.
- **No receiving.** Consuming events from a rule is the `event` feature, described with the other triggers.
