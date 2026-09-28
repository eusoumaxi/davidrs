//! DynamoDB helpers for the parts every data layer gets wrong, and nothing
//! else.
//!
//! Keys, projections, conditions and item shapes stay in your code, written
//! with the SDK's own builders and [`serde_dynamo`]. There is no table
//! abstraction and no mapper: they would hide exactly what matters, which
//! items the service actually read or accepted.
//!
//! - **Partial outcomes.** `BatchWriteItem` and `BatchGetItem` succeed while
//!   returning work they did not do; [`batch_write`] and [`batch_get`] retry
//!   only that work, with backoff, and report what is still undone.
//! - **Bounded reads.** [`query_bounded`] and [`scan_bounded`] stop at an item
//!   cap, a page cap or the deadline, and say which.
//! - **Page tokens.** [`encode_cursor`] and [`decode_cursor`] turn a
//!   `LastEvaluatedKey` into an opaque string a client can hand back.
//! - **Tracing.** [`span`] names a call the way X-Ray's service graph expects.
//! - **Conditional writes.** [`is_conditional_failure`] tells a lost race from
//!   a failed call.
//!
//! The [DynamoDB chapter](crate::guide::dynamodb) of the guide shows each one
//! in use.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::types::{AttributeValue, KeysAndAttributes, WriteRequest};
use base64::Engine as _;
use serde_json::{Map, Value};
use tracing::Instrument as _;

use crate::{Deadline, RuntimeError};

/// A DynamoDB item.
pub type Item = HashMap<String, AttributeValue>;

/// The largest batch `BatchWriteItem` accepts.
pub const MAX_BATCH_WRITE_REQUESTS: usize = 25;

/// The largest batch `BatchGetItem` accepts.
pub const MAX_BATCH_GET_KEYS: usize = 100;

/// The pause before the first retry of unprocessed work.
const FIRST_BACKOFF: Duration = Duration::from_millis(25);

/// Waits before retry number `retry` (from 1) of unprocessed work.
///
/// The pause doubles from 25 ms up to 1.6 s, as the service asks, and never
/// runs past the deadline.
async fn back_off(retry: usize, deadline: Deadline) {
    let pause = FIRST_BACKOFF * (1 << (retry - 1).min(6));
    tokio::time::sleep(pause.min(deadline.remaining())).await;
}

/// The client span for one DynamoDB call.
///
/// It is named `DynamoDB.<operation>` and carries the OpenTelemetry database
/// attributes, which X-Ray shows as the DynamoDB node of the service graph.
/// The helpers in this module wrap their own requests with it; wrap yours the
/// same way:
///
/// ```no_run
/// # async fn read(client: aws_sdk_dynamodb::Client, table: String) {
/// use tracing::Instrument as _;
/// let output = client
///     .get_item()
///     .table_name(&table)
///     .send()
///     .instrument(davidrs::dynamo::span("GetItem", &table))
///     .await;
/// # }
/// ```
#[must_use]
pub fn span(operation: &str, table: &str) -> tracing::Span {
    tracing::info_span!(
        "dynamodb",
        "otel.name" = %format_args!("DynamoDB.{operation}"),
        "otel.kind" = "CLIENT",
        "db.system.name" = "aws.dynamodb",
        "db.operation.name" = operation,
        "aws.dynamodb.table_names" = table,
    )
}

/// Whether a failed call was refused by its condition expression
/// (`ConditionalCheckFailedException`), for any operation that takes one.
///
/// A refused condition is usually an answer, not a fault: the item already
/// exists, the lease is held by someone else, the version moved on.
///
/// ```no_run
/// # async fn claim(client: aws_sdk_dynamodb::Client) -> Result<bool, davidrs::RuntimeError> {
/// use aws_sdk_dynamodb::types::AttributeValue;
///
/// let result = client
///     .put_item()
///     .table_name("orders")
///     .item("id", AttributeValue::S("order-1".to_owned()))
///     .condition_expression("attribute_not_exists(id)")
///     .send()
///     .await;
/// match result {
///     Ok(_) => Ok(true),
///     Err(error) if davidrs::dynamo::is_conditional_failure(&error) => Ok(false),
///     Err(error) => Err(davidrs::RuntimeError::other("claiming order-1", error)),
/// }
/// # }
/// ```
#[must_use]
pub fn is_conditional_failure<E, R>(error: &SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata,
{
    error
        .as_service_error()
        .and_then(ProvideErrorMetadata::code)
        == Some("ConditionalCheckFailedException")
}

/// An item as a JSON object, without the named attributes.
///
/// Use it to return a stored item without its physical keys:
/// `to_object(item, &["PK", "SK"])`.
///
/// # Errors
///
/// Returns [`RuntimeError`] when an attribute cannot be represented as JSON,
/// such as a binary value.
pub fn to_object(mut item: Item, without: &[&str]) -> Result<Map<String, Value>, RuntimeError> {
    for name in without {
        item.remove(*name);
    }
    serde_dynamo::aws_sdk_dynamodb_1::from_item(item)
        .map_err(|error| RuntimeError::other("converting an item to JSON", error))
}

/// A `LastEvaluatedKey` as an opaque page token.
///
/// The token is the base64 of the key's JSON, with attributes in name order,
/// so one key always spells one token.
///
/// # Errors
///
/// Returns [`RuntimeError`] when the key cannot be represented as JSON, such
/// as a binary attribute.
///
/// # Examples
///
/// ```
/// use aws_sdk_dynamodb::types::AttributeValue;
/// let key = [("SK", "b"), ("PK", "a")]
///     .map(|(k, v)| (k.to_owned(), AttributeValue::S(v.to_owned())))
///     .into_iter()
///     .collect();
/// let token = davidrs::dynamo::encode_cursor(key).expect("token");
/// assert_eq!(token, "eyJQSyI6ImEiLCJTSyI6ImIifQ==");
/// assert!(davidrs::dynamo::decode_cursor(&token).is_some());
/// ```
pub fn encode_cursor(key: Item) -> Result<String, RuntimeError> {
    let key: BTreeMap<String, Value> = serde_dynamo::aws_sdk_dynamodb_1::from_item(key)
        .map_err(|error| RuntimeError::other("encoding a page token", error))?;
    let json = Value::Object(key.into_iter().collect()).to_string();
    Ok(base64::engine::general_purpose::STANDARD.encode(json))
}

/// A page token back into an exclusive start key.
///
/// Returns `None` for anything that is not a token [`encode_cursor`] wrote: a
/// client that sends garbage starts from the first page rather than failing.
/// The token is readable and editable; prefer [`encode_cursor_signed`] for
/// tokens that leave the service.
#[must_use]
pub fn decode_cursor(token: &str) -> Option<Item> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(token.as_bytes())
        .ok()?;
    let key: Map<String, Value> = serde_json::from_slice(&bytes).ok()?;
    serde_dynamo::aws_sdk_dynamodb_1::to_item(key).ok()
}

/// The secret that signs page tokens.
///
/// Read it once at cold start — from Secrets Manager, say — and keep it in
/// the application state. Changing it invalidates every token in flight,
/// which only sends their clients back to the first page.
pub struct CursorSecret(ring::hmac::Key);

impl CursorSecret {
    /// A key from secret bytes; 32 random bytes are enough.
    #[must_use]
    pub fn new(secret: &[u8]) -> Self {
        Self(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret))
    }
}

impl std::fmt::Debug for CursorSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CursorSecret(..)")
    }
}

/// URL-safe base64 without padding: a signed token goes into a query string
/// as it is.
const URL_SAFE: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A `LastEvaluatedKey` as a signed page token that a client can hold and
/// read but not change.
///
/// The token is the key's JSON and its HMAC-SHA256 under `secret`, both in
/// URL-safe base64. A forged or edited token fails verification in
/// [`decode_cursor_signed`] and reads as "first page", so a client cannot
/// move a scan to a key of its choosing or turn a malformed key into a
/// server error. It is not encrypted: the key's values are only encoded.
///
/// # Errors
///
/// Returns [`RuntimeError`] when the key cannot be represented as JSON, such
/// as a binary attribute.
///
/// # Examples
///
/// ```
/// use aws_sdk_dynamodb::types::AttributeValue;
/// use davidrs::dynamo::{decode_cursor_signed, encode_cursor_signed, CursorSecret};
///
/// let secret = CursorSecret::new(b"a secret read at cold start, 32 bytes");
/// let key = [("PK".to_owned(), AttributeValue::S("ORDER#7".to_owned()))].into_iter().collect();
/// let token = encode_cursor_signed(key, &secret).expect("token");
/// assert!(decode_cursor_signed(&token, &secret).is_some());
/// assert!(decode_cursor_signed(&format!("{token}x"), &secret).is_none());
/// ```
pub fn encode_cursor_signed(key: Item, secret: &CursorSecret) -> Result<String, RuntimeError> {
    let key: BTreeMap<String, Value> = serde_dynamo::aws_sdk_dynamodb_1::from_item(key)
        .map_err(|error| RuntimeError::other("encoding a page token", error))?;
    let json = Value::Object(key.into_iter().collect()).to_string();
    let tag = ring::hmac::sign(&secret.0, json.as_bytes());
    Ok(format!(
        "{}.{}",
        URL_SAFE.encode(&json),
        URL_SAFE.encode(tag.as_ref())
    ))
}

/// A signed page token back into an exclusive start key, or `None` when the
/// token was not signed with `secret` or was changed since.
///
/// The signature is checked in constant time, before the key is parsed.
#[must_use]
pub fn decode_cursor_signed(token: &str, secret: &CursorSecret) -> Option<Item> {
    let (payload, tag) = token.split_once('.')?;
    let json = URL_SAFE.decode(payload).ok()?;
    let tag = URL_SAFE.decode(tag).ok()?;
    ring::hmac::verify(&secret.0, &json, &tag).ok()?;
    let key: Map<String, Value> = serde_json::from_slice(&json).ok()?;
    serde_dynamo::aws_sdk_dynamodb_1::to_item(key).ok()
}

/// Limits for a paginated read, built with [`PageLimits::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageLimits {
    /// The largest number of items to return.
    pub max_items: usize,
    /// The largest number of requests to issue.
    pub max_pages: usize,
}

impl PageLimits {
    /// Explicit item and page caps.
    #[must_use]
    pub const fn new(max_items: usize, max_pages: usize) -> Self {
        Self {
            max_items,
            max_pages,
        }
    }
}

/// The result of a bounded read.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Page {
    /// The items read, in service order.
    pub items: Vec<Item>,
    /// The key to resume from, when a limit stopped the read early.
    ///
    /// `None` when the read is complete, and also when it stopped before its
    /// first page, since there is nothing to resume after.
    pub next: Option<Item>,
    /// Which limit stopped the read, if any.
    pub stopped_by: Option<Stop>,
}

impl Page {
    /// Whether every matching item was read.
    ///
    /// Treating an incomplete read as a complete one is the mistake these
    /// helpers exist to prevent, so this is `false` whenever a limit stopped
    /// the read, with or without a key to resume from.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.next.is_none() && self.stopped_by.is_none()
    }
}

/// Why a bounded read stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Stop {
    /// The item cap was reached.
    Items,
    /// The page cap was reached.
    Pages,
    /// The deadline was reached.
    Deadline,
}

/// One page request of a bounded read: what the service returned and where
/// to resume.
struct PageResponse {
    items: Vec<Item>,
    next: Option<Item>,
}

/// The page size to request when `remaining` items are still wanted, never
/// more than the caller's own `limit`.
///
/// Asking for no more than the cap means a page never has to be cut short, so
/// the service's `LastEvaluatedKey` is always an exact place to resume.
fn page_limit(own: Option<i32>, remaining: usize) -> i32 {
    let cap = i32::try_from(remaining).unwrap_or(i32::MAX).max(1);
    own.map_or(cap, |own| own.min(cap))
}

/// The paging loop shared by [`query_bounded`] and [`scan_bounded`].
///
/// `fetch` receives the start key, the page number and how many items are
/// still wanted. A page with more items than that is cut to the cap and
/// reported as [`Stop::Items`].
async fn read_bounded<F, Fut>(
    limits: PageLimits,
    deadline: Deadline,
    mut fetch: F,
) -> Result<Page, RuntimeError>
where
    F: FnMut(Option<Item>, usize, usize) -> Fut,
    Fut: std::future::Future<Output = Result<PageResponse, RuntimeError>>,
{
    let mut items: Vec<Item> = Vec::new();
    let mut start: Option<Item> = None;
    if limits.max_items == 0 {
        return Ok(Page {
            items,
            next: None,
            stopped_by: Some(Stop::Items),
        });
    }
    for page in 0..limits.max_pages {
        if deadline.is_expired() {
            return Ok(Page {
                items,
                next: start,
                stopped_by: Some(Stop::Deadline),
            });
        }
        let remaining = limits.max_items.saturating_sub(items.len());
        let response = match deadline.run(fetch(start.clone(), page, remaining)).await {
            Ok(response) => response?,
            Err(_) => {
                return Ok(Page {
                    items,
                    next: start,
                    stopped_by: Some(Stop::Deadline),
                });
            }
        };
        items.extend(response.items);
        start = response.next;
        if start.is_none() && items.len() <= limits.max_items {
            return Ok(Page {
                items,
                next: None,
                stopped_by: None,
            });
        }
        if items.len() >= limits.max_items {
            items.truncate(limits.max_items);
            return Ok(Page {
                items,
                next: start,
                stopped_by: Some(Stop::Items),
            });
        }
    }
    Ok(Page {
        items,
        next: start,
        stopped_by: Some(Stop::Pages),
    })
}

/// Runs a query to completion or to an explicit limit.
///
/// `build` is called with the exclusive start key for each page, so the caller
/// keeps full control of the table, index, key condition and projection. Each
/// request's `Limit` is lowered to the number of items still wanted, so the
/// item cap never cuts a page short and [`Page::next`] resumes exactly after
/// the last item returned. The deadline bounds each request in flight; a
/// timed-out page is not consumed. A zero item cap sends no request.
///
/// # Errors
///
/// Returns [`RuntimeError`] when a page request fails. Pages already read are
/// discarded; use a smaller `max_items` if partial progress matters.
pub async fn query_bounded<B>(
    limits: PageLimits,
    deadline: Deadline,
    mut build: B,
) -> Result<Page, RuntimeError>
where
    B: FnMut(Option<Item>) -> aws_sdk_dynamodb::operation::query::builders::QueryFluentBuilder,
{
    read_bounded(limits, deadline, |start, page, remaining| {
        let request = build(start);
        let limit = page_limit(*request.get_limit(), remaining);
        let request = request.limit(limit);
        let table = request.get_table_name().clone().unwrap_or_default();
        async move {
            let response = request
                .send()
                .instrument(span("Query", &table))
                .await
                .map_err(|error| RuntimeError::other(format!("query page {page}"), error))?;
            Ok(PageResponse {
                items: response.items.unwrap_or_default(),
                next: response.last_evaluated_key,
            })
        }
    })
    .await
}

/// Runs a scan to completion or to an explicit limit.
///
/// The same contract as [`query_bounded`]: `build` receives the exclusive
/// start key, each request's `Limit` is capped at the items still wanted, and
/// a limit that stops the scan early is reported, not hidden.
///
/// # Errors
///
/// Returns [`RuntimeError`] when a page request fails.
pub async fn scan_bounded<B>(
    limits: PageLimits,
    deadline: Deadline,
    mut build: B,
) -> Result<Page, RuntimeError>
where
    B: FnMut(Option<Item>) -> aws_sdk_dynamodb::operation::scan::builders::ScanFluentBuilder,
{
    read_bounded(limits, deadline, |start, page, remaining| {
        let request = build(start);
        let limit = page_limit(*request.get_limit(), remaining);
        let request = request.limit(limit);
        let table = request.get_table_name().clone().unwrap_or_default();
        async move {
            let response = request
                .send()
                .instrument(span("Scan", &table))
                .await
                .map_err(|error| RuntimeError::other(format!("scan page {page}"), error))?;
            Ok(PageResponse {
                items: response.items.unwrap_or_default(),
                next: response.last_evaluated_key,
            })
        }
    })
    .await
}

/// The outcome of a bounded batch write.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct BatchWriteOutcome {
    /// How many requests the service accepted.
    pub written: usize,
    /// Requests not written: those the last attempt returned as unprocessed,
    /// then those never sent because the attempts or the deadline ran out.
    ///
    /// Non-empty means the write is **incomplete**.
    pub unprocessed: Vec<WriteRequest>,
    /// How many round trips were made.
    pub attempts: usize,
}

impl BatchWriteOutcome {
    /// Whether every request was written.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unprocessed.is_empty()
    }
}

/// Writes put and delete requests in batches of [`MAX_BATCH_WRITE_REQUESTS`],
/// retrying only the ones the service left unprocessed.
///
/// `BatchWriteItem` succeeds while returning requests it did not apply. This
/// sends them again first, after a doubling pause, and reports whatever is
/// still unwritten instead of reporting success. `max_attempts` bounds the
/// round trips of the whole call, not of each batch: writing 60 requests takes
/// at least three. The deadline bounds every request, including retries.
///
/// # Errors
///
/// Returns [`RuntimeError`] when a request fails or times out in flight.
/// Accepted writes stay written; a timed-out write may also have completed
/// remotely. Do not treat that error as proof that nothing was written.
pub async fn batch_write(
    client: &Client,
    table: &str,
    requests: Vec<WriteRequest>,
    max_attempts: usize,
    deadline: Deadline,
) -> Result<BatchWriteOutcome, RuntimeError> {
    let total = requests.len();
    let mut outcome = BatchWriteOutcome::default();
    let mut pending: Vec<_> = requests;
    let mut retry = 0;

    while !pending.is_empty() && outcome.attempts < max_attempts {
        if retry > 0 {
            back_off(retry, deadline).await;
        }
        if deadline.is_expired() {
            break;
        }
        outcome.attempts += 1;
        let chunk: Vec<_> = pending
            .drain(..pending.len().min(MAX_BATCH_WRITE_REQUESTS))
            .collect();
        let sent = chunk.len();
        let response = deadline
            .run(
                client
                    .batch_write_item()
                    .request_items(table, chunk)
                    .send()
                    .instrument(span("BatchWriteItem", table)),
            )
            .await?
            .map_err(|error| RuntimeError::other(format!("batch write to {table}"), error))?;
        let returned = response
            .unprocessed_items
            .unwrap_or_default()
            .remove(table)
            .unwrap_or_default();
        outcome.written += sent - returned.len();
        retry = if returned.is_empty() { 0 } else { retry + 1 };
        let mut next = returned;
        next.extend(pending);
        pending = next;
    }
    outcome.unprocessed = pending;
    debug_assert!(outcome.written + outcome.unprocessed.len() <= total);
    Ok(outcome)
}

/// The outcome of a bounded batch read.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct BatchGetOutcome {
    /// The items found, in the order the service returned them. A key with
    /// no item is simply absent.
    pub items: Vec<Item>,
    /// Keys not read: those the last attempt of a batch returned as
    /// unprocessed, then every key of the batches after it.
    ///
    /// Non-empty means the read is **incomplete**.
    pub unprocessed: Vec<Item>,
}

impl BatchGetOutcome {
    /// Whether every key was read.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unprocessed.is_empty()
    }
}

/// Reads items by key, [`MAX_BATCH_GET_KEYS`] at a time, retrying only the keys
/// the service left unprocessed.
///
/// Each batch gets up to `max_attempts` requests, with a doubling pause
/// between them; the deadline bounds every in-flight request. When a batch
/// runs out of attempts or time between requests, its keys and every later batch's keys are
/// reported in [`BatchGetOutcome::unprocessed`]. `configure` sets anything else on
/// each batch, such as a projection or consistent reads.
///
/// Keys must be distinct: the service rejects a batch that repeats one.
///
/// # Errors
///
/// Returns [`RuntimeError`] when a request fails or times out in flight, or when
/// `configure` leaves the batch without keys.
pub async fn batch_get<F>(
    client: &Client,
    table: &str,
    keys: Vec<Item>,
    max_attempts: usize,
    deadline: Deadline,
    configure: F,
) -> Result<BatchGetOutcome, RuntimeError>
where
    F: Fn(
        aws_sdk_dynamodb::types::builders::KeysAndAttributesBuilder,
    ) -> aws_sdk_dynamodb::types::builders::KeysAndAttributesBuilder,
{
    let mut read = BatchGetOutcome::default();
    let mut batches = keys.chunks(MAX_BATCH_GET_KEYS).map(<[Item]>::to_vec);
    while let Some(mut pending) = batches.next() {
        for attempt in 0..max_attempts {
            if pending.is_empty() {
                break;
            }
            if attempt > 0 {
                back_off(attempt, deadline).await;
            }
            if deadline.is_expired() {
                break;
            }
            let request = configure(KeysAndAttributes::builder().set_keys(Some(pending)))
                .build()
                .map_err(|error| RuntimeError::other("building a batch read", error))?;
            let mut response = deadline
                .run(
                    client
                        .batch_get_item()
                        .request_items(table, request)
                        .send()
                        .instrument(span("BatchGetItem", table)),
                )
                .await?
                .map_err(|error| RuntimeError::other(format!("batch read from {table}"), error))?;
            read.items.extend(
                response
                    .responses
                    .as_mut()
                    .and_then(|responses| responses.remove(table))
                    .unwrap_or_default(),
            );
            pending = response
                .unprocessed_keys
                .unwrap_or_default()
                .remove(table)
                .map(|unprocessed| unprocessed.keys)
                .unwrap_or_default();
        }
        if !pending.is_empty() {
            read.unprocessed.extend(pending);
            read.unprocessed.extend(batches.flatten());
            break;
        }
    }
    Ok(read)
}
