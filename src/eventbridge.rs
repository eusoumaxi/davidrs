//! Publishing to EventBridge with per-entry outcomes.
//!
//! `PutEvents` answers HTTP 200 while rejecting individual entries, so code
//! that checks only the SDK `Result` reports success for events that were
//! never accepted. Every function here reports the outcome of each input.
//!
//! A route is plain configuration: a `const` with a source and a detail type.
//! There is no event trait to implement and no publisher object to build; the
//! payload only needs [`Serialize`](serde::Serialize).
//!
//! The [EventBridge chapter](crate::guide::eventbridge) of the guide shows
//! both functions in use.

use aws_sdk_eventbridge::Client;
use aws_sdk_eventbridge::types::PutEventsRequestEntry;

use crate::{Deadline, RuntimeError};

/// The largest batch `PutEvents` accepts.
pub const MAX_ENTRIES: usize = 10;

/// Where an event goes: its source and detail type.
///
/// ```
/// use davidrs::eventbridge::EventRoute;
///
/// const ORDER_CREATED: EventRoute = EventRoute {
///     source: "example.orders",
///     detail_type: "order.created",
/// };
/// assert_eq!(ORDER_CREATED.detail_type, "order.created");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRoute {
    /// The publishing service, e.g. `example.orders`.
    pub source: &'static str,
    /// The event name, e.g. `order.created`.
    pub detail_type: &'static str,
}

/// What happened to one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryOutcome {
    /// EventBridge accepted it and assigned this id.
    Accepted(String),
    /// EventBridge rejected it with this code and message.
    Rejected {
        /// The service error code.
        code: String,
        /// The service message.
        message: String,
    },
    /// The call did not complete in time, or the service reported nothing
    /// for this entry, so acceptance is unknown.
    ///
    /// Publishing it again may duplicate the event; it must not be assumed
    /// lost.
    Unknown,
}

impl EntryOutcome {
    /// Whether EventBridge confirmed acceptance.
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }
}

/// The outcome of one publish call, one entry per input, in input order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublishOutcome {
    /// Per-entry results, aligned with the inputs.
    pub entries: Vec<EntryOutcome>,
}

impl PublishOutcome {
    /// Whether every entry was accepted.
    #[must_use]
    pub fn all_accepted(&self) -> bool {
        self.entries.iter().all(EntryOutcome::is_accepted)
    }

    /// The indexes of entries that were not accepted.
    #[must_use]
    pub fn failed_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, outcome)| !outcome.is_accepted())
            .map(|(index, _)| index)
            .collect()
    }
}

/// Publishes one event.
///
/// The call runs under `deadline`; if the deadline arrives first, the entry
/// is [`EntryOutcome::Unknown`].
///
/// # Errors
///
/// Returns [`RuntimeError`] when the payload cannot be serialized or the call
/// fails. A *rejected* entry is not an error: inspect the outcome.
///
/// # Examples
///
/// ```no_run
/// # async fn created(client: aws_sdk_eventbridge::Client, deadline: davidrs::Deadline) -> Result<(), davidrs::RuntimeError> {
/// use davidrs::eventbridge::{self, EventRoute};
///
/// const ORDER_CREATED: EventRoute = EventRoute {
///     source: "example.orders",
///     detail_type: "order.created",
/// };
///
/// let detail = serde_json::json!({ "orderId": "order-1" });
/// let outcome = eventbridge::publish(&client, "default", &ORDER_CREATED, &detail, deadline).await?;
/// if !outcome.all_accepted() {
///     return Err(davidrs::RuntimeError::message("order.created was not accepted"));
/// }
/// # Ok(())
/// # }
/// ```
pub async fn publish<T: serde::Serialize>(
    client: &Client,
    bus: &str,
    route: &EventRoute,
    detail: &T,
    deadline: Deadline,
) -> Result<PublishOutcome, RuntimeError> {
    publish_batch(client, bus, route, std::slice::from_ref(detail), deadline).await
}

/// Publishes up to [`MAX_ENTRIES`] events of the same route in one call.
///
/// The outcome has one entry per detail, in order. When the deadline arrives
/// before the call completes, every entry is [`EntryOutcome::Unknown`]; an
/// empty slice makes no call.
///
/// # Errors
///
/// Returns [`RuntimeError::LimitExceeded`] when given more than
/// [`MAX_ENTRIES`] details, and another [`RuntimeError`] when a detail cannot
/// be serialized or the call fails.
pub async fn publish_batch<T: serde::Serialize>(
    client: &Client,
    bus: &str,
    route: &EventRoute,
    details: &[T],
    deadline: Deadline,
) -> Result<PublishOutcome, RuntimeError> {
    if details.len() > MAX_ENTRIES {
        return Err(RuntimeError::LimitExceeded {
            kind: "event entries",
            limit: MAX_ENTRIES as u64,
        });
    }
    if details.is_empty() {
        return Ok(PublishOutcome {
            entries: Vec::new(),
        });
    }
    let mut entries = Vec::with_capacity(details.len());
    for detail in details {
        let json = serde_json::to_string(detail)
            .map_err(|error| RuntimeError::other("serializing an event detail", error))?;
        entries.push(
            PutEventsRequestEntry::builder()
                .event_bus_name(bus)
                .source(route.source)
                .detail_type(route.detail_type)
                .detail(json)
                .build(),
        );
    }

    let request = client.put_events().set_entries(Some(entries)).send();
    let response = match deadline.run(request).await {
        Err(_) => {
            return Ok(PublishOutcome {
                entries: vec![EntryOutcome::Unknown; details.len()],
            });
        }
        Ok(Err(error)) => return Err(RuntimeError::other(format!("publishing to {bus}"), error)),
        Ok(Ok(response)) => response,
    };

    let results = response.entries.unwrap_or_default();
    let outcomes = (0..details.len())
        .map(|index| match results.get(index) {
            Some(result) => match (result.event_id(), result.error_code()) {
                (Some(id), None) => EntryOutcome::Accepted(id.to_owned()),
                (_, Some(code)) => EntryOutcome::Rejected {
                    code: code.to_owned(),
                    message: result.error_message().unwrap_or_default().to_owned(),
                },
                (None, None) => EntryOutcome::Unknown,
            },
            None => EntryOutcome::Unknown,
        })
        .collect();
    Ok(PublishOutcome { entries: outcomes })
}
