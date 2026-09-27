# Outbound HTTP

[`client::build`](crate::client::build) builds an ordinary [`reqwest::Client`] with a connect timeout, a whole-request timeout, no automatic redirects and the Mozilla roots compiled in. [`read_bounded`](crate::client::read_bounded) and [`json_bounded`](crate::client::json_bounded) read a response body up to a byte cap. [`send_error`](crate::client::send_error) turns a transport error into a [`RuntimeError`](crate::RuntimeError) without the request URL. There is no wrapper type: after `build` you write plain `reqwest`.

## Why it exists

Each of these defaults prevents a failure that only shows up in production:

- **No timeout.** `reqwest::Client::new()` waits forever. A hung upstream then holds the invocation until Lambda kills it, so no error is logged and no cleanup runs. [`Limits`](crate::client::Limits) makes both limits explicit: 2 s to connect and 10 s for the whole request by default.
- **Unbounded bodies.** `response.bytes()` and `response.json()` buffer whatever the server sends. One oversized or endless response exhausts the function's memory and kills the execution environment. The bounded readers refuse a declared `Content-Length` over the cap before reading anything, then check every chunk before keeping it, so a missing or lying header does not help.
- **Redirects.** `reqwest` follows up to ten by default. It drops `Authorization` when the host changes, but a custom credential header such as `x-api-key` goes wherever the upstream points, and each hop spends budget the caller did not plan for. Here a `3xx` is returned to the caller as it is.
- **URLs in errors.** A `reqwest::Error` prints its URL, and some APIs take a key as a query parameter. One `error!("{error}")` then writes the key into the logs. `send_error` and the bounded readers strip the URL and keep the cause.

## How to use it

Build the client once, in `main`, and keep it in the application state. It holds a connection pool, so a warm invocation reuses the connection the previous one opened.

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
