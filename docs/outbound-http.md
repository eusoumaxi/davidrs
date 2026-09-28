# Outbound HTTP

Enable `client`. [`client::build`](crate::client::build) returns an ordinary `reqwest::Client`. There is no wrapper. Build it once in `main`, store it on `App`, and a warm invocation reuses the connection pool.

Three defaults of `reqwest::Client::new()` are the wrong ones inside a Lambda:

- **A timeout.** Without one, a hung upstream holds the invocation until Lambda kills it. Nothing is logged and no cleanup runs. [`Limits`](crate::client::Limits) is 2 seconds to connect and 10 seconds for the whole request unless you pass other values.
- **A byte cap.** `response.bytes()` and `response.json()` will buffer whatever the server sends, including a body that never ends. [`read_bounded`](crate::client::read_bounded) and [`json_bounded`](crate::client::json_bounded) refuse a declared `Content-Length` over the cap before reading, then check every chunk. Going over the cap is [`RuntimeError::LimitExceeded`](crate::RuntimeError::LimitExceeded), never a truncated value you might treat as complete.
- **No redirects, and no URL in the error.** `reqwest` follows up to ten redirects by default. It drops `Authorization` when the host changes, and it does not drop a custom credential header such as `x-api-key`, so that header would go wherever the upstream points. A `reqwest` error also prints its URL, which is how a key in a query string reaches CloudWatch. [`send_error`](crate::client::send_error) keeps the cause and drops the URL. Here a `3xx` is returned to you as it is.

The readers do not look at the HTTP status. A `404` body is still a body. Decide what the status means, then read. Build URLs from configuration, never from request input: this client has no host allowlist.

## How to use it

```rust
use std::time::Duration;

use davidrs::client::{self, Limits};

let http = client::build(Limits::new(Duration::from_secs(1), Duration::from_secs(5)))?;
# let _ = http;
# Ok::<(), davidrs::RuntimeError>(())
```

Then every call follows the same three steps: send and map the transport error, decide what the status means, read the body under a cap.

```rust,no_run
use davidrs::client::{json_bounded, send_error};
use davidrs::RuntimeError;
use serde::Deserialize;

/// The exchange rate the upstream answers.
#[derive(Debug, Deserialize)]
struct Rate {
    currency: String,
    value: f64,
}

async fn rate(http: &reqwest::Client, key: &str) -> Result<Rate, RuntimeError> {
    let response = http
        .get("https://rates.example.com/latest")
        .query(&[("currency", "EUR"), ("key", key)])
        .send()
        .await
        .map_err(|error| send_error("fetching the rate", error))?;
    if !response.status().is_success() {
        return Err(RuntimeError::message(format!(
            "the rate service answered {}",
            response.status()
        )));
    }
    json_bounded(response, 16 * 1024).await
}
```

The readers never look at the status. A `404` or a `500` body is read like any other, because what a status means is the caller's decision, and an error body is often the only diagnostic there is. Use [`read_bounded`](crate::client::read_bounded) when the body is not JSON.

A body over the cap is [`RuntimeError::LimitExceeded`](crate::RuntimeError::LimitExceeded), never a truncated value. Choose the cap from what the endpoint can legitimately return: a few kilobytes for a single record, more for a page of results.

## Use cases

- Calling a partner API that takes its key in the query string, without the key ever reaching a log.
- Fetching a document whose size you do not control, such as a JWKS, a feed or a webhook target's answer, with a hard ceiling on memory.
- Giving one upstream a shorter budget than the invocation: a separate client with a 2 s request limit for a call that is optional to the response.
- Refusing to be redirected into another host, or into a loopback address the function can reach but the caller should not.

## What it does not do

- **No retries.** Whether a request is safe to repeat depends on the endpoint. Retry in the caller, under the invocation [`Deadline`](crate::Deadline).
- **No status handling.** Non-`2xx` answers are not errors here.
- **No allowlist of hosts.** The client calls whatever URL it is given. Build URLs from configuration, not from request input.
- **No per-call deadline.** The request limit is per client. To bound one call by the invocation budget, wrap it in `Deadline::run`, which every trigger feature provides.
