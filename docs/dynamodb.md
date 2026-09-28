# DynamoDB

Enable `dynamo`. Build the client once in `main` with [`aws::sdk_config`](crate::aws::sdk_config), as [AWS configuration](crate::guide::aws_config) shows, and keep it on `App`.

The [`dynamo`](crate::dynamo) module is a handful of functions around the DynamoDB SDK for the parts every data layer gets wrong: batches that succeed while doing only part of the work, paginated reads with no upper bound, page tokens, tracing spans and conditional-write failures. Keys, projections, conditions and item shapes stay in your code, written with the SDK's builders. There is no repository trait and no item mapper, because either one would hide which items were actually read or accepted. Check [`Page::is_complete`](crate::dynamo::Page::is_complete) and the batch outcome, not only the SDK `Result`. Keep the tenant in the query's key condition. A page token, even a signed one, is not a permission.

## Why it exists

- **Partial batches read as success.** `BatchWriteItem` and `BatchGetItem` answer `200` while returning the requests they did not process. Code that checks only the `Result` loses writes and misses items. [`batch_write`](crate::dynamo::batch_write) and [`batch_get`](crate::dynamo::batch_get) send that work again, after a pause that doubles from 25 ms, and report what is still undone.
- **Unbounded reads.** A loop that follows `LastEvaluatedKey` until it is absent reads a whole partition, however large it has grown, and runs past the deadline. [`query_bounded`](crate::dynamo::query_bounded) and [`scan_bounded`](crate::dynamo::scan_bounded) stop at an item cap, a page cap or the deadline, and say which.
- **Incomplete reads treated as complete.** A read that stopped early and does not say so is the same bug as a lost write. [`Page::is_complete`](crate::dynamo::Page::is_complete) is `false` whenever a limit stopped the read.
- **Forgeable page tokens.** Handing `LastEvaluatedKey` to a client as raw JSON exposes the key schema and invites edited tokens. [`encode_cursor_signed`](crate::dynamo::encode_cursor_signed) signs the token with a [`CursorSecret`](crate::dynamo::CursorSecret), and [`decode_cursor_signed`](crate::dynamo::decode_cursor_signed) treats anything it did not sign — edited, forged or garbage — as "start from the first page".

## How to use it

Build the client once in `main` with [`aws::sdk_config`](crate::aws::sdk_config), keep it in the application state, and call the helpers with it.

### Bounded reads and page tokens

`query_bounded` calls your closure once per page with the key to start from; the closure returns the SDK's own query builder. [`PageLimits`](crate::dynamo::PageLimits) caps the items returned and the requests made. Each request's `Limit` is lowered to the number of items still wanted, so a page never has to be cut, and [`Page::next`](crate::dynamo::Page::next) is always an exact place to resume.

```rust,no_run
use aws_sdk_dynamodb::types::AttributeValue;
use davidrs::dynamo::{self, CursorSecret, PageLimits};
use davidrs::{Deadline, RuntimeError};

/// One page of a user's orders, and the signed token for the next one.
async fn orders(
    client: &aws_sdk_dynamodb::Client,
    secret: &CursorSecret,
    user: &str,
    token: Option<&str>,
    deadline: Deadline,
) -> Result<(Vec<serde_json::Map<String, serde_json::Value>>, Option<String>, bool), RuntimeError> {
    let mut start = token.and_then(|token| dynamo::decode_cursor_signed(token, secret));
    let page = dynamo::query_bounded(PageLimits::new(50, 5), deadline, |resume| {
        client
            .query()
            .table_name("orders")
            .key_condition_expression("PK = :user")
            .expression_attribute_values(":user", AttributeValue::S(format!("USER#{user}")))
            .set_exclusive_start_key(resume.or_else(|| start.take()))
    })
    .await?;
    let complete = page.is_complete();
    let next = page
        .next
        .map(|key| dynamo::encode_cursor_signed(key, secret))
        .transpose()?;
    let items = page
        .items
        .into_iter()
        .map(|item| dynamo::to_object(item, &["PK", "SK"]))
        .collect::<Result<_, _>>()?;
    Ok((items, next, complete))
}
```

Pass `user` from the authorized caller or tenant in the handler's `Grant`, never directly from a query parameter. Return the completion flag with the items and token so a deadline before the first page cannot look like an empty result.

The first call receives `None` and starts from the client's token; every later call receives the key the service returned. [`Page::stopped_by`](crate::dynamo::Page::stopped_by) tells the three stops apart: [`Stop::Items`](crate::dynamo::Stop::Items) is a full page, [`Stop::Pages`](crate::dynamo::Stop::Pages) means the pages came back short, because a filter expression discarded items or a page reached the service's 1 MB size, and [`Stop::Deadline`](crate::dynamo::Stop::Deadline) means the invocation ran short. A read that stops before its first page has no key to resume from, so `next` is `None` there, and `is_complete` is still `false`.

Use the signed pair for any token a client holds: the token carries the key's JSON and its HMAC-SHA256 under a secret the function reads at cold start, in URL-safe base64, so an edited or forged token is refused before its key is parsed and the client simply starts over. The signature makes the token tamper-proof, not secret: the key's values are only encoded. The unsigned [`encode_cursor`](crate::dynamo::encode_cursor) and [`decode_cursor`](crate::dynamo::decode_cursor) write the base64 of the key's JSON with attributes in name order, so one key always gives one token. That token is opaque, not secret: a client can read the key inside it and hand back an edited one. Keep the tenant in the query's key condition, never only in the token: a query returns items of the partition its condition names whatever start key it is given, so an edited token can at most move the read within that partition, or fail the request. A scan has no such boundary: apply the tenant as a filter there, and treat `decode_cursor` as a position, never as a permission. [`to_object`](crate::dynamo::to_object) turns an item into a JSON object without the attributes you name, typically the physical keys:

```rust
use aws_sdk_dynamodb::types::AttributeValue;
use davidrs::dynamo::{self, Item};

let item = Item::from([
    ("PK".to_owned(), AttributeValue::S("USER#1".to_owned())),
    ("SK".to_owned(), AttributeValue::S("ORDER#7".to_owned())),
    ("total".to_owned(), AttributeValue::N("42".to_owned())),
]);
let key: Item = item
    .iter()
    .filter(|(name, _)| name.as_str() != "total")
    .map(|(name, value)| (name.clone(), value.clone()))
    .collect();

let token = dynamo::encode_cursor(key.clone())?;
assert_eq!(dynamo::decode_cursor(&token), Some(key));
assert_eq!(dynamo::decode_cursor("edited by hand"), None);

let object = dynamo::to_object(item, &["PK", "SK"])?;
assert_eq!(serde_json::Value::Object(object), serde_json::json!({ "total": 42 }));
# Ok::<(), davidrs::RuntimeError>(())
```

### Batches

`batch_write` splits the requests into batches of [`MAX_BATCH_WRITE_REQUESTS`](crate::dynamo::MAX_BATCH_WRITE_REQUESTS) (25). Requests the service returns go first in the next call, after the pause. `max_attempts` bounds the round trips of the whole call, so writing 60 requests needs at least three. Whatever is not written when the attempts or the deadline run out, returned or never sent, is in [`BatchWriteOutcome::unprocessed`](crate::dynamo::BatchWriteOutcome::unprocessed):

```rust,no_run
use aws_sdk_dynamodb::types::{AttributeValue, PutRequest, WriteRequest};
use davidrs::{Deadline, RuntimeError};

async fn save(client: &aws_sdk_dynamodb::Client, ids: &[String], deadline: Deadline) -> Result<(), RuntimeError> {
    let requests = ids
        .iter()
        .map(|id| {
            let put = PutRequest::builder()
                .item("PK", AttributeValue::S(format!("ORDER#{id}")))
                .item("SK", AttributeValue::S("SUMMARY".to_owned()))
                .build()
                .map_err(|error| RuntimeError::other("building a put", error))?;
            Ok(WriteRequest::builder().put_request(put).build())
        })
        .collect::<Result<Vec<_>, RuntimeError>>()?;
    let outcome = davidrs::dynamo::batch_write(client, "orders", requests, 8, deadline).await?;
    if !outcome.is_complete() {
        return Err(RuntimeError::message(format!(
            "{} of {} orders not saved",
            outcome.unprocessed.len(),
            ids.len()
        )));
    }
    Ok(())
}
```

`batch_get` reads [`MAX_BATCH_GET_KEYS`](crate::dynamo::MAX_BATCH_GET_KEYS) (100) keys per request and gives each batch up to `max_attempts` requests. Its last argument configures every batch, for a projection or a consistent read: `|batch| batch.consistent_read(true)`. When a batch runs out of attempts or time, its keys and those of every later batch are in [`BatchGetOutcome::unprocessed`](crate::dynamo::BatchGetOutcome::unprocessed); a key with no item is simply absent from `items`. Keys must be distinct, as the service rejects a batch that repeats one.

### Conditional writes and spans

[`is_conditional_failure`](crate::dynamo::is_conditional_failure) is `true` when a call failed only because its condition expression did not hold: the item already exists, the version moved on. That is usually an answer to act on, not an error to report. It works for any operation that takes a condition.

[`span`](crate::dynamo::span) is the client span for one call, named `DynamoDB.<operation>` with the OpenTelemetry database attributes, which X-Ray draws as the DynamoDB node of the service graph. The helpers above use it for their own requests; wrap your own calls with `.instrument(dynamo::span("GetItem", &table))`.

## Use cases

- A listing endpoint that answers 50 items and a `next` token, and never reads more than five pages to do it.
- Importing a file of a few thousand records with `batch_write`, reporting exactly which records still need writing.
- Loading the items behind a list of ids with `batch_get` in one pass.
- Claiming a job with a conditional put, where losing the race is a normal outcome.

## What it does not do

- **No repository and no mapper.** Keys, conditions and item shapes differ in every table, and a layer that hides them also hides which items were actually read or written. Use the SDK's builders and [`serde_dynamo`] for item shapes.
- **No transactions and no single-item helpers.** `GetItem`, `PutItem`, `UpdateItem` and `TransactWriteItems` are one SDK call each.
- **No cancellation of remote writes.** Deadlines bound requests in flight. A timed-out query returns an incomplete page; a timed-out batch returns `RuntimeError::DeadlineExceeded`. A write may have completed remotely, so retry only when doing so is safe.
- **No resuming a failed read.** When a page request fails, the pages already read are discarded and the error is returned. Keep `max_items` small if partial progress matters.

## If the read looks complete and is not

| What you see | What it usually means | What to change |
| --- | --- | --- |
| `BatchWriteItem` returned `Ok` and items are missing | The service answered `200` and listed unprocessed items | Use [`batch_write`](crate::dynamo::batch_write) and read the outcome. Do not check only the SDK `Result`. |
| A listing is short and the client cannot ask for the rest | The read stopped at a limit and the response omitted `next`, or treated the page as complete | Return the token from [`Page::next`](crate::dynamo::Page::next) whenever [`is_complete`](crate::dynamo::Page::is_complete) is `false`. [`Stop::Pages`](crate::dynamo::Stop::Pages) means a filter or the 1 MB page size, not "there is nothing left". |
| An edited page token reads another tenant's items | The tenant lived only in the token | Put the tenant in the key condition. A signed token is tamper-proof, not a permission. [`decode_cursor_signed`](crate::dynamo::decode_cursor_signed) treats a forged token as "start at the first page". |
| The query runs until Lambda kills the function | The loop follows `LastEvaluatedKey` with no cap | Use [`query_bounded`](crate::dynamo::query_bounded) or [`scan_bounded`](crate::dynamo::scan_bounded) with [`PageLimits`](crate::dynamo::PageLimits) and the invocation deadline. |
