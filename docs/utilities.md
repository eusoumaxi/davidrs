# Utilities

Three small capabilities that most functions need sooner or later: an in-process cache, gzip with a cap on the decoded size, and SHA-256 digests. Each has its own feature, so a function links only the ones it uses.

## In-process caches (`cache`)

[`cache`](crate::cache) re-exports [Moka]'s asynchronous [`Cache`](crate::cache::Cache), its [`CacheBuilder`](crate::cache::CacheBuilder) and [`EvictionPolicy`](crate::cache::EvictionPolicy). There is no wrapper: Moka's own documentation applies as written.

[Moka]: https://docs.rs/moka

### Why it exists

A `static` `HashMap` behind a lock is the usual first cache, and it goes wrong in three ways: it grows until the execution environment runs out of memory, its entries never go stale, and ten concurrent misses on one key start ten loads. Moka bounds the size, expires entries by time, and runs one load per key while the other callers wait for its result. The re-export pins one version of that API for every function built on this crate.

### When an in-process cache is right

Lambda runs one invocation at a time in an execution environment and reuses the environment while it is warm, for minutes or hours. A cache kept in the application state therefore survives from one warm invocation to the next. It is also private to that environment, empty after every cold start, and invisible to the other environments serving traffic at the same moment.

That makes it right for values that are the same for every caller and may be stale for a bounded time: reference data, configuration read from a service, an access token for a downstream API, the result of an expensive lookup. It is wrong for anything that must agree across environments, such as rate limits, counters, sessions or idempotency records. Those belong in a shared store.

### How to use it

Build the cache with the application state, bound it by entry count and time to live, and load through `get_with`, so concurrent misses share one load.

```rust
use std::sync::Arc;
use std::time::Duration;

use davidrs::cache::Cache;

/// Everything built once per execution environment.
struct App {
    rates: Cache<String, Arc<f64>>,
}

async fn fetch_rate(_currency: &str) -> f64 {
    1.08
}

async fn rate(app: &App, currency: &str) -> Arc<f64> {
    app.rates
        .get_with(currency.to_owned(), async { Arc::new(fetch_rate(currency).await) })
        .await
}

# #[tokio::main]
# async fn main() {
let app = App {
    rates: Cache::builder()
        .max_capacity(1_000)
        .time_to_live(Duration::from_secs(300))
        .build(),
};
assert_eq!(*rate(&app, "EUR").await, 1.08);
# }
```

When the load can fail, use `try_get_with`: the error is returned to every waiting caller and nothing is cached, so the next call tries again. Whether a value is worth caching at all, such as an empty result, is one comparison at the insertion site.

### Use cases

- The signing keys, feature flags or price list a function reads on every invocation but that change a few times a day.
- An OAuth access token for a partner API, cached a little shorter than its lifetime.
- A lookup that costs a database round trip, keyed by its normalized parameters.

### What it does not do

- **No sharing.** Each execution environment has its own copy, and nothing invalidates it when the source changes. Choose the time to live for the staleness you can accept.
- **No persistence.** A cold start begins empty.
- **No background work.** Moka removes expired entries while the cache is used; between invocations the environment is frozen and nothing runs.

## Bounded gzip (`compression`)

[`gzip`](crate::compression::gzip) compresses bytes. [`gunzip_bounded`](crate::compression::gunzip_bounded) and [`gunzip_to_string_bounded`](crate::compression::gunzip_to_string_bounded) decompress, refusing to produce more than a given number of bytes.

### Why it exists

`read_to_end` on a gzip decoder is a decompression bomb: a few kilobytes of input can expand to gigabytes and take the execution environment down with it. Here the cap is on the **decoded** size, whatever the compressed size. Going over it is [`RuntimeError::LimitExceeded`](crate::RuntimeError::LimitExceeded), never a silently truncated value that would be mistaken for the whole payload.

### How to use it

```rust
use davidrs::compression::{gunzip_bounded, gunzip_to_string_bounded, gzip};
use davidrs::RuntimeError;

let document = r#"{"orders":["order-1","order-2"]}"#;
let packed = gzip(document.as_bytes())?;
assert_eq!(gunzip_to_string_bounded(&packed, 64 * 1024)?, document);

let bomb = gzip(&vec![0; 1024 * 1024])?;
assert!(bomb.len() < 4 * 1024);
assert!(matches!(
    gunzip_bounded(&bomb, 64 * 1024),
    Err(RuntimeError::LimitExceeded { kind: "decoded bytes", .. })
));
# Ok::<(), RuntimeError>(())
```

Choose the cap from the largest value your code can use, not from the size of the input.

### Use cases

- Large JSON documents stored compressed to fit a record size limit, read back with a cap.
- A request or message body that arrives gzip-encoded from a client you do not control.
- Compressing a payload before putting it on a queue or bus with a message size limit.

### What it does not do

- **No streaming.** Input and output are whole buffers.
- **One format.** gzip only; no raw deflate, zlib, brotli or zstd.
- **Every member.** A stream of concatenated gzip members is decoded whole, and the cap applies to all of them together.

## Digests (`digest`)

[`sha256`](crate::digest::sha256) returns the 32-byte SHA-256 digest, [`hex`](crate::digest::hex) spells bytes as lowercase hex, and [`sha256_hex`](crate::digest::sha256_hex) does both.

### Why it exists

A digest that becomes a key must be spelled the same way everywhere it is computed. Upper- against lowercase hex, or a missing leading zero, and two functions derive two keys for one thing. These functions fix the spelling: lowercase, two digits per byte. SHA-256 comes from `ring`, which the TLS stack already links, so no second crypto crate is added.

### How to use it

```rust
use davidrs::digest::{hex, sha256, sha256_hex};

let key = sha256_hex(br#"{"currency":"EUR","day":"2026-09-27"}"#);
assert_eq!(key.len(), 64);
assert_eq!(key, hex(&sha256(br#"{"currency":"EUR","day":"2026-09-27"}"#)));
assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
```

Hash a canonical form: the same request serialized with its fields in another order is another digest.

### Use cases

- An idempotency key derived from the request body.
- A cache key from normalized query parameters.
- A content-addressed object name, so identical uploads share one object.

### What it does not do

- **No secrets.** SHA-256 is neither a MAC nor a password hash. Use HMAC to authenticate a message and a key derivation function for passwords.
- **No constant-time comparison.** Comparing two digests with `==` can leak timing; that matters only when one of them is secret.
