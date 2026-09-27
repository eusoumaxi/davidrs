# Invocations

Every adapter turns what Lambda delivers into the same few values: an [`Invocation`](crate::Invocation) with its [`Deadline`](crate::Deadline), a [`Context`](crate::Context) for the handler, and a [`RuntimeError`](crate::RuntimeError) when something outside the request goes wrong. They live at the crate root and need no feature, so a domain crate can take a `Deadline` or return a `RuntimeError` without linking the runtime.

## Deadline

**What it is.** An absolute point in time after which work must stop, held as a monotonic [`Instant`](std::time::Instant). Lambda announces each invocation's deadline; the adapter converts it once, and everything else derives from that one value.

**Why it exists.** A timeout restarted per call does not add up: three attempts of five seconds each will overrun a ten-second invocation, and Lambda then stops the process in the middle of a write, with no response and no cleanup. A `Deadline` only ever shrinks. [`child`](crate::Deadline::child) gives a shorter budget that never outlives its parent, [`with_margin`](crate::Deadline::with_margin) keeps time in hand for cleanup and never moves the deadline later, and a retry derives its budget from the same parent instead of getting a fresh allowance.

**How to use it.** Bound every await that could stall with [`Deadline::run`](crate::Deadline::run):

```rust
use std::time::Duration;

use davidrs::{Deadline, RuntimeError};

async fn fetch_price(_attempt: u32) -> Result<u32, String> {
    Ok(42)
}

# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), RuntimeError> {
let invocation = Deadline::in_from_now(Duration::from_secs(10));
let work = invocation.with_margin(Duration::from_millis(500));
let mut price = None;
for attempt in 0..3 {
    let call = work.child(Duration::from_secs(2));
    if let Ok(Ok(value)) = call.run(fetch_price(attempt)).await {
        price = Some(value);
        break;
    }
}
assert_eq!(price, Some(42));
assert!(work.child(Duration::from_secs(60)) <= work);
# Ok(())
# }
```

**Use cases.**

- A retry loop whose attempts share one budget.
- An outbound call bounded by whatever is left of the invocation, capped at its own limit with `child`.
- Releasing a lease or saving progress after the main work: run the work under `with_margin`, the cleanup under the full deadline.
- Handing a stream producer its own budget, so it stops even while a client keeps reading.

**What it does not do.** A deadline cancels local waiting only: the future is dropped at its next await point, and a remote write that was already sent may still complete. Synchronous code between awaits is not interrupted. Anything that must survive cancellation belongs outside the bounded future.

## Invocation and Context

**What it is.** [`Invocation`](crate::Invocation) is the identity and timing of one invocation: the request id, the raw `X-Amzn-Trace-Id` header, the invoked function ARN, the deadline, and when the adapter received it. A handler gets it inside a [`Context<Scope>`](crate::Context), next to the scope its caller was authorized with — `()` for triggers without a policy.

**Why it exists.** Every trigger answers the same questions — which request is this, how long is left, which trace does it belong to — so every handler reads them from the same type instead of the trigger's native context. [`runtime::invocation_from`](crate::runtime::invocation_from) does the conversion for every adapter, including the case a hand-written conversion gets wrong: a local emulator sends a relative budget (`600000`) instead of an epoch, which read as an epoch would expire every invocation at once. Values up to Lambda's 15-minute maximum are read as "that long from now".

**How to use it.**

```rust
use std::time::Duration;

use davidrs::{Context, Deadline, Invocation};

fn describe(context: &Context<()>) -> String {
    let invocation = context.invocation();
    format!(
        "request {} (trace {}), {} ms left",
        invocation.request_id,
        invocation.trace_root().unwrap_or("none"),
        context.deadline().remaining().as_millis(),
    )
}

let invocation = Invocation::new("r-1", Deadline::in_from_now(Duration::from_secs(3)))
    .with_trace_id(Some("Root=1-abc;Parent=def;Sampled=1".to_owned()));
let context = Context::new(invocation, ());
assert!(describe(&context).starts_with("request r-1 (trace 1-abc)"));
```

**Use cases.**

- Correlating log lines and metrics by request id.
- Returning [`trace_root`](crate::Invocation::trace_root) in a response header, so a client can quote the trace when it reports a problem.
- Measuring a long poll from [`started`](crate::Invocation::started) rather than from when the handler began.
- Building a context by hand in a unit test (see the `test_support` feature).

**What it does not do.** The invocation does not carry the rest of the native
context (client context, identity pool). `Context` does not vouch for its
scope: the scope's own type does. Keep a scope's fields private, and only your
policy can build one.

## RuntimeError and error_chain

**What it is.** [`RuntimeError`](crate::RuntimeError) is the one error of startup and infrastructure paths: missing configuration, an exhausted budget, a bounded limit reached, or anything else with its source kept. [`error_chain`](crate::error_chain) formats an error and every cause behind it as one line.

**Why it exists.** An error logged by its outermost message alone says "loading configuration" and hides the reason. `RuntimeError::other` keeps the cause reachable through `source()`, and `error_chain` prints all of it, once. It carries no HTTP status and no SDK type, so the value types stay dependency-free.

**How to use it.**

```rust
use davidrs::{error_chain, RuntimeError};

fn load() -> Result<String, RuntimeError> {
    std::fs::read_to_string("/nonexistent/settings.json")
        .map_err(|error| RuntimeError::other("loading settings", error))
}

let error = load().expect_err("the file does not exist");
assert_eq!(error.to_string(), "loading settings");
assert!(error_chain(&error).starts_with("loading settings: "));
```

**Use cases.**

- The error type of `main`, so a failed cold start names its cause.
- The handler error of a non-HTTP trigger: its message becomes the invocation error Lambda records.
- Wrapping an SDK error with what the code was doing when it failed.

**What it does not do.** It is not an HTTP answer: a request failure is an [`http::Failure`](crate::http::Failure), which knows its status and keeps its detail out of the response. Match on it with a `_` arm, because variants are added over time.

## Configuration

**What it is.** [`required_env`](crate::required_env), [`optional_env`](crate::optional_env) and [`list_env`](crate::list_env) read the environment variables a function is configured with.

**Why it exists.** Configuration read lazily fails the first request that needs it, long after the deploy looked healthy. Read in `main`, a missing variable fails the cold start instead. The error names the variable and never echoes a value, because values are often ARNs, URLs or credentials. `optional_env` treats a blank value as unset, and `list_env` trims each entry, so `"https://a.example.com, https://b.example.com"` allows the second origin rather than `" https://b.example.com"`.

**How to use it.**

```rust
use davidrs::{list_env, optional_env, required_env, RuntimeError};

/// What the function reads once, before its first invocation.
struct Config {
    table: String,
    origins: Vec<String>,
    greeting: String,
}

impl Config {
    fn from_env() -> Result<Self, RuntimeError> {
        Ok(Self {
            table: required_env("TABLE")?,
            origins: list_env("ALLOWED_ORIGINS")?,
            greeting: optional_env("GREETING").unwrap_or_else(|| "Hello".to_owned()),
        })
    }
}

# std::env::set_var("TABLE", "orders");
# std::env::set_var("ALLOWED_ORIGINS", "https://a.example.com, https://b.example.com");
let config = Config::from_env()?;
assert_eq!(config.table, "orders");
assert_eq!(config.origins, ["https://a.example.com", "https://b.example.com"]);
assert_eq!(config.greeting, "Hello");
# Ok::<(), RuntimeError>(())
```

**Use cases.**

- A table or queue name the function cannot run without.
- An allowlist of origins or tenants.
- An optional setting with a default.

**What it does not do.** It reads strings only: parse numbers and durations yourself, and report a bad one as a [`RuntimeError::Configuration`](crate::RuntimeError::Configuration). Nothing is cached or reloaded; read once and keep the result in your application state. Secrets belong in a secret store, not in the environment.
