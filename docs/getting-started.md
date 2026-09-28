# Getting started

This chapter builds one HTTP function from an empty crate, runs it on your machine, and shows the JSON a caller receives when the call works and when it does not. After it, you should be able to pick the Cargo features for a different trigger and know which chapter to open next.

You do not need an AWS account for anything in this chapter. Deployment, IAM and API Gateway are in [Deployment](crate::guide::deployment).

## What you need

- Rust 1.94.1 or later, the oldest version the crate supports (`rust-version`). The repository develops on the version pinned in `rust-toolchain.toml`; a function of your own can use any stable compiler from 1.94.1 on.
- [Cargo Lambda](https://www.cargo-lambda.info), only when you want the local emulator. Install it with `brew install cargo-lambda/tap/cargo-lambda` (that also installs Zig) or `pip3 install cargo-lambda`.
- A working knowledge of `async fn` and of a `Cargo.toml`. You do not need to know the Lambda Runtime API. The crate speaks it for you.

## Create the function

```bash
cargo lambda new --http hello
cd hello
```

`cargo lambda new --http` writes a binary crate whose name is the function name.
Replace its generated `[dependencies]` section with the following; the template
does not include `davidrs`. Keep the `[package]` section:

```toml
[dependencies]
davidrs = { version = "0.1", default-features = false, features = ["http", "logs"] }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Three choices in that manifest are easy to get wrong later:

- **`default-features = false`.** The crate's defaults are already empty. Writing it anyway makes the manifest say so, and keeps a future default from arriving in your binary unannounced.
- **`features = ["http", "logs"]`.** `http` is the buffered API Gateway and Function URL pipeline. `logs` is plain-text logging. Leave out anything this function does not call. The [features](crate::guide::features) chapter lists what each name links. If `Api` does not resolve, `http` is missing from this list.
- **Tokio's `macros` and `rt-multi-thread`.** `#[tokio::main]` needs both. `lambda_runtime` enables them already, but name them in your own manifest so the function does not depend on another crate's feature set.

## The program

Replace `src/main.rs` with the program below. It is the whole function: configuration read once at cold start, one handler, one pipeline.

```rust,no_run
use std::sync::Arc;

use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request};
use davidrs::{Context, RuntimeError};
use serde::{Deserialize, Serialize};

/// Everything the function builds once, before the first invocation.
struct App {
    greeting: String,
}

impl App {
    /// Reads configuration. A missing variable fails the cold start, not a request.
    fn from_env() -> Result<Self, RuntimeError> {
        Ok(Self {
            greeting: davidrs::optional_env("GREETING").unwrap_or_else(|| "Hello".to_owned()),
        })
    }
}

/// `GET /hello/{name}`: the path parameter, decoded.
#[derive(Deserialize)]
struct HelloPath {
    name: String,
}

#[derive(Serialize)]
struct Greeting {
    message: String,
}

async fn hello(app: Arc<App>, input: HelloPath, _context: Context<()>) -> Result<Json<Greeting>, Failure> {
    Ok(Json(Greeting {
        message: format!("{}, {}", app.greeting, input.name),
    }))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("hello")?;
    let app = Arc::new(App::from_env()?);
    Api::new("hello", Public, PlainErrors)
        .run(app, |request: &Request| request.path::<HelloPath>(), hello)
        .await
}
```

Read it in the order the process actually runs.

**`App::from_env` runs once, in `main`, before the loop.** Lambda reuses the process for later invocations, so clients, table names and this greeting are built at cold start and shared as `Arc<App>`. [`optional_env`](crate::optional_env) treats a missing or blank `GREETING` as unset and the function falls back to `"Hello"`. A value the function cannot run without is [`required_env`](crate::required_env): the error names the variable, never the value, and the cold start fails. That is what you want. A missing variable discovered on the first real request looks like a healthy deploy until someone calls it.

**`telemetry::init("hello")` installs logging.** With `otel`, the string is the fallback service name when neither `OTEL_SERVICE_NAME` nor `AWS_LAMBDA_FUNCTION_NAME` is set. Bind the guard to `_telemetry` until `main` returns so the trace provider stays alive. With `logs` alone the global log subscriber stays installed for the process lifetime. [Telemetry](crate::guide::telemetry) covers both modes.

**`Api::new` is the pipeline, configured once.** The three arguments are the only policy most first functions need:

| Argument | Here | What it decides |
| --- | --- | --- |
| `"hello"` | the operation name | Appears in logs and traces. It is not the URL path. |
| [`Public`](crate::http::Public) | the [`Policy`](crate::http::Policy) | Everyone may call. The handler's scope is `()`. A route that must know the caller uses [`Access`](crate::http::access::Access) or its own policy; [Access control](crate::guide::access) shows both. |
| [`PlainErrors`](crate::http::PlainErrors) | the [`ErrorRenderer`](crate::http::ErrorRenderer) | A failure becomes `{"errorCode","errorMessage"}`. [`ProblemErrors`](crate::http::ProblemErrors) (feature `problem`) writes RFC 9457 problem details instead. |

**The closure is the decoder, and it runs before the handler.** `|request| request.path::<HelloPath>()` reads the path parameters API Gateway or the local emulator extracted and deserializes them into `HelloPath`. It is synchronous. A path that does not fit the struct becomes a `400` and `hello` never runs. A JSON body uses `request.json::<T>()` instead; the [HTTP chapter](crate::guide::http) lists every reader and the status each one returns.

**`hello` is the only business logic.** It receives the shared `App`, the decoded path, and a [`Context`](crate::Context). The context carries the request id, the trace and the [deadline](crate::guide::invocations). This handler ignores the context because it does no I/O. A handler that calls DynamoDB, another HTTP service, or Secrets Manager should bound that await with the deadline, or a retry can outlive the invocation. It returns [`Json`](crate::http::Json) on success and a [`Failure`](crate::http::Failure) otherwise. Build a client-visible rejection with `Failure::new`. Build a server failure with `Failure::internal` or `Failure::from_error`: the detail is available for a deliberate diagnostic and the client receives the fixed message `InternalServerError`.

**`Api::run` starts the Lambda loop and does not return** until the runtime itself fails. `main`'s `Result` is that failure, not a request failure. A request failure is a response.

## What a caller receives

With `GREETING` unset, `GET /hello/world` is HTTP 200 and this body:

```json
{"message":"Hello, world"}
```

`Json` chooses status `200` and `Content-Type: application/json`. A created resource is `(StatusCode::CREATED, Json(...))`. A delete that has nothing to return is [`NoContent`](crate::http::NoContent). The [HTTP chapter](crate::guide::http) lists the conversions.

A path that does not deserialize, or a JSON body that is not JSON, never reaches `hello`. The renderer writes the failure. With `PlainErrors` a bad path is HTTP 400:

```json
{"errorCode":"ERROR_INVALID_PATH","errorMessage":"Invalid request path"}
```

A handler that returns `Failure::internal("FAULT_STORE", "table orders is missing")` is HTTP 500, and the body is always:

```json
{"errorCode":"FAULT_STORE","errorMessage":"InternalServerError"}
```

The table name is not in that body. It is in the failure's internal detail, which the pipeline does not render and does not log. Log it yourself, on purpose, if a diagnostic needs it. This split is the point of [`Failure`](crate::http::Failure): a `500` built from `format!("{error}")` is how a table name, an address or an upstream message reaches a client.

## Choose features for the function you actually have

Start from what invokes the function, then add only what the handler calls.

| The function… | Features | Next chapter |
| --- | --- | --- |
| answers API Gateway or a Function URL with one response | `http` | [HTTP](crate::guide::http) |
| answers through a response stream (JSON or server-sent events) | `http-stream` | [Streaming](crate::guide::streaming) |
| also receives REST API events (payload format 1.0) | add `apigw-rest` | [Deployment](crate::guide::deployment) |
| sits behind an Application Load Balancer | add `alb` | [HTTP](crate::guide::http) |
| consumes an SQS queue | `queue` (and `queue-visibility` to delay a retry) | [Queues](crate::guide::queues) |
| is an EventBridge target, or runs on a schedule | `event` or `schedule` | [Triggers](crate::guide::triggers) |
| is invoked directly with a JSON payload | `runtime` | [Triggers](crate::guide::triggers) |
| reads DynamoDB, publishes events, or reads secrets | `dynamo`, `eventbridge`, or `secrets` (each brings `aws`) | the chapter of that service |
| calls other HTTP services | `client` | [Outbound HTTP](crate::guide::outbound_http) |
| verifies bearer tokens itself, because nothing in front does | `auth` | [Bearer tokens](crate::guide::tokens) |
| logs, emits metrics, or traces to X-Ray | `logs`, `metrics`, `otel` | [Telemetry](crate::guide::telemetry) |

In a Cargo workspace, features are unified across every member being built. A missing feature can compile anyway because a sibling enabled it, and then fail when that function is built alone for deployment. Check each function the way it will be deployed:

```bash
cargo check -p hello
```

`cargo lambda build --package hello` compiles that same package alone.

## Run it locally

In production the `{name}` path parameter comes from the API Gateway route. The local emulator does not know your routes unless you tell it. Add this to the function's `Cargo.toml`:

```toml
[package.metadata.lambda.watch.router]
"/hello/{name}" = "hello"
```

The left side is the path you will `curl`. The right side is the binary name.
See Cargo Lambda's [HTTP project creation](https://www.cargo-lambda.info/commands/new.html)
and [local routing](https://www.cargo-lambda.info/commands/watch.html#custom-http-routes).

Open the function directory in VS Code or Cursor and install the
`rust-lang.rust-analyzer` extension. It reads this function's enabled Cargo
features automatically. Run `cargo check` in the integrated terminal before
starting the emulator; no AWS account is needed for this example.

```bash
cargo lambda watch                      # leave this terminal running
```

In a second terminal:

```bash
curl http://localhost:9000/hello/world    # {"message":"Hello, world"}
```

`cargo lambda watch` serves binary crates only. The programs under this repository's `examples/` are examples, not binaries of a watchable crate. To run one, copy it into `src/main.rs` of a crate made with `cargo lambda new`, with the same features. [examples/README.md](https://github.com/eusoumaxi/davidrs/blob/main/examples/README.md) says which features each program needs. `cargo check --example http --features http` compiles an example in this repository without running it.

Two differences from Lambda matter, and both surprise people:

- **The deadline is ten minutes, whatever you configured.** The emulator sends a relative budget of `600000` instead of an epoch timestamp. The crate reads any budget up to Lambda's 15-minute maximum as "that long from now". A function that times out in 3 seconds in AWS will not time out locally. Exercise a short deadline in a test with [`test_support`](crate::test_support), as [Testing](crate::guide::testing) shows.
- **The emulator adds permissive CORS and answers preflights itself.** The headers you see on a local response are not the headers your [`Cors`](crate::http::stream::Cors) allowlist will send. Start the emulator with `cargo lambda watch --disable-cors` when you are checking CORS.

## If it does not work

| What you see | What it usually means | What to change |
| --- | --- | --- |
| `Api` or `telemetry` is unresolved | The feature is not enabled for this package | Add `http` or `logs` to this function's `features`, then `cargo check -p <function>` |
| `cargo lambda watch` does nothing useful inside the `davidrs` repository | Watch serves binaries, and the examples are not | Copy the example into a crate from `cargo lambda new`, or `curl` the function you just created |
| `curl` returns a response but the path parameter is empty or the status is 400 `ERROR_INVALID_PATH` | The emulator has no route for `/hello/{name}` | Add `[package.metadata.lambda.watch.router]` as above, and restart `watch` |
| The function deploys, then the first request fails on a missing setting | Configuration is read inside the handler | Read it in `from_env`, called from `main`, with `required_env` |
| API Gateway returns `502` with `{"message":"Internal server error"}` and your renderer never ran | The handler returned `Err` as an invocation error, or the function timed out before the pipeline could answer | Return `Failure` from an HTTP handler. Set the function timeout below the gateway's, and let the pipeline render `504` `ERROR_TIMEOUT` |
| A change compiles in the workspace and fails in CI or in `cargo lambda build` | Another package enabled the feature you forgot | `cargo check -p` that function alone |

## Where to go next

| You want to | Read |
| --- | --- |
| Understand the order of steps, and where a policy plugs in | [Architecture](crate::guide::architecture) |
| Require a caller, a tenant, or a quota | [Access control](crate::guide::access) |
| Put CloudFront, WAF and Cognito in front, and know what is left for the function | [Security on AWS](crate::guide::aws_security) |
| Build the zip and connect the function | [Deployment](crate::guide::deployment) |
| Test the handler without the emulator | [Testing](crate::guide::testing) |
