# Telemetry

Logs, metrics and traces are three separate features, because a Lambda binary pays at cold start for everything it links. A function that emits metrics does not link an exporter, and a function that only logs does not link OpenTelemetry.

| Feature | Gives | Links |
| --- | --- | --- |
| `logs` | [`init`](crate::telemetry::init), [`logs`](crate::telemetry::logs): a plain-text subscriber, invocation spans, timed startup | `tracing`, `tracing-subscriber` (fmt only) |
| `metrics` | [`Metrics`](crate::telemetry::Metrics): CloudWatch Embedded Metric Format | `serde_json` |
| `otel` | `logs` plus X-Ray traces sent to the sandbox's X-Ray agent | OpenTelemetry, the OTLP protobuf types |

## Installing telemetry: `init` and `Guard`

**What it is.** [`telemetry::init`](crate::telemetry::init) installs the process's global `tracing` subscriber: plain-text logs, and with `otel` also the layer that turns spans into X-Ray traces. It returns a [`Guard`](crate::telemetry::Guard) that `main` holds until it returns.

**Why it exists.** A global subscriber can be installed once per process. A library that installs one from a constructor takes that decision away from the binary, and the second installer fails or silently loses. Here the call is explicit, happens once in `main`, and reports a second attempt as an error instead of ignoring it.

**How to use it.** Call it first in `main`, before any pipeline starts:

```rust
fn main() -> Result<(), davidrs::RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders")?;
    tracing::info!("ready");
    Ok(())
}
```

The `service` argument names the traces only when neither `OTEL_SERVICE_NAME` nor `AWS_LAMBDA_FUNCTION_NAME` is set, which happens outside Lambda. Keep the guard for the life of `main`.

**Use cases.**

- Every Lambda binary with the `logs` feature: one line in `main`.
- A local run or an integration test, where the fallback service name identifies the traces.

**What it does not do.** It does not configure anything else: no JSON log format, no per-module filters, no export of logs or metrics over OpenTelemetry. A binary that needs a different stack builds its own subscriber from the parts below instead of calling `init`.

## Logs

**What it is.** [`logs::init`](crate::telemetry::logs::init) installs a plain-text subscriber at the level [`level_from_env`](crate::telemetry::logs::level_from_env) reads from `RUST_LOG`. Its format, [`text_layer`](crate::telemetry::logs::text_layer), has no ANSI colours and no timestamp, because CloudWatch shows escape codes verbatim and Lambda already stamps every line.

**Why it exists.** `tracing-subscriber`'s `env-filter` understands directives such as `info,my_crate=debug`, and pulls in `regex` for it, a large dependency for a Lambda binary. A bare level (`debug`, `WARN`, `off`, or `0` to `5`) covers what a function sets in its configuration, and anything else falls back to `INFO` instead of failing to start. An empty `RUST_LOG` reads as `ERROR`, as `tracing` parses it.

**How to use it.** [`telemetry::init`](crate::telemetry::init) already does this. To assemble a subscriber of your own, reuse the pieces:

```rust
use davidrs::telemetry::logs;
use tracing_subscriber::layer::SubscriberExt as _;

let subscriber = tracing_subscriber::registry()
    .with(logs::level_from_env())
    .with(logs::text_layer());
tracing::subscriber::with_default(subscriber, || {
    tracing::info!(order_id = "o-1", "accepted");
});
```

**Use cases.**

- `RUST_LOG=debug` on one function while you investigate it, without a redeploy of the code.
- A subscriber that adds your own layer next to the standard format.

**What it does not do.** No per-target filtering, no JSON output and no sampling of log lines.

## Timed startup: `initialize`

**What it is.** [`logs::initialize`](crate::telemetry::logs::initialize) awaits one startup step, logs an `initialized` event with the step's `component`, `init_ms` and `success`, and returns the step's result unchanged.

**Why it exists.** Cold starts are where Lambda latency hides, and the slow step is rarely the one you would guess. Timing each step by hand invites logging the value it produced or the error it failed with, and either may hold configuration or a secret. `initialize` logs only the name, the duration and whether it succeeded.

**How to use it.** Wrap each step before the function starts serving:

```rust
use davidrs::telemetry::logs::initialize;

struct Settings {
    table: String,
}

async fn load_settings() -> Result<Settings, std::io::Error> {
    Ok(Settings { table: "orders".to_owned() })
}

# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), std::io::Error> {
let settings = initialize("settings", load_settings()).await?;
assert_eq!(settings.table, "orders");
# Ok(())
# }
```

**Use cases.**

- Loading configuration or a secret at cold start.
- Building SDK clients, warming a cache, or fetching signing keys before the first request.

**What it does not do.** It does not retry, time out or cache: the step is yours, and so is its error.

## Invocation spans

**What it is.** [`logs::invocation_span`](crate::telemetry::logs::invocation_span) is the `lambda.invocation` span every pipeline opens around one invocation, carrying the operation name and the request id. With `otel` it is also the server span of the invocation's X-Ray trace.

**Why it exists.** Log lines from concurrent invocations interleave in one log stream, and a line without its request id cannot be traced back. Because every pipeline opens the same span, every event inside a handler carries the request id without the handler passing it around, and your own spans nest under it.

**How to use it.** The pipelines open it for you; create spans inside your handler and they become its children. A custom adapter, or a test, opens it the same way:

```rust
use std::time::Duration;

use davidrs::telemetry::logs::invocation_span;
use davidrs::{Deadline, Invocation};

let invocation = Invocation::new("request-1", Deadline::in_from_now(Duration::from_secs(3)));
let span = invocation_span("get-order", &invocation);
span.in_scope(|| tracing::info!("loading the order"));
```

**Use cases.**

- Finding every line of one failed request by its request id.
- Nesting `tracing::info_span!("load_order")` inside a handler to time one step.

**What it does not do.** It records no request or response content, and no user identity: add those fields yourself where they are safe to log.

## Metrics: CloudWatch EMF

**What it is.** [`Metrics`](crate::telemetry::Metrics) builds one Embedded Metric Format document: a namespace, one dimension set, metrics with a [`Unit`](crate::telemetry::Unit), and searchable properties. Printed as one log line, CloudWatch Logs turns it into metrics.

**Why it exists.** Calling `PutMetricData` puts a network round trip on the request path; a log line costs nothing extra. But CloudWatch rejects a document with more than 100 metrics ([`MAX_METRICS`](crate::telemetry::metrics::MAX_METRICS)) or more than 30 dimensions in a set ([`MAX_DIMENSIONS`](crate::telemetry::metrics::MAX_DIMENSIONS)), and a rejected document loses every metric in it without an error anywhere. `Metrics` ignores a new metric or dimension past those limits instead, so the rest still arrive, and recording a name again updates its value rather than declaring it twice.

**How to use it.**

```rust
use std::time::SystemTime;

use davidrs::telemetry::{Metrics, Unit};

let line = Metrics::new("Shop")
    .dimension("Operation", "checkout")
    .metric("Latency", 12.5, Unit::Milliseconds)
    .metric("Items", 3.0, Unit::Count)
    .property("orderId", "o-1")
    .to_json(SystemTime::now())?;
println!("{line}");
# Ok::<(), davidrs::RuntimeError>(())
```

Print the line with `println!`, not through a log subscriber that prefixes it: CloudWatch reads a line as EMF only when the whole line is the JSON document.

**Use cases.**

- Latency and item counts per operation, graphed and alarmed on without an SDK client.
- Counting business events, such as orders accepted or payments declined, by dimension.
- Attaching a request id as a property, so a metric spike leads back to the log line.

**What it does not do.** One document has one dimension set, the units offered are `Count`, `Milliseconds`, `Bytes`, `Percent` and `None`, and there is no aggregation, buffering or high-resolution storage: each call is one document. A value that is not finite is written as `null`, which CloudWatch does not accept as a metric value.

## X-Ray traces

**What it is.** With `otel`, [`telemetry::init`](crate::telemetry::init) adds a layer that turns `tracing` spans into OpenTelemetry spans and exports each one, as it ends, to the X-Ray agent Lambda runs in every sandbox. The pieces are public:

- [`tracer_provider`](crate::telemetry::tracer_provider) builds the provider that sends one UDP datagram per span to an agent address.
- [`agent_endpoint`](crate::telemetry::agent_endpoint) is that address: `AWS_XRAY_DAEMON_ADDRESS`, or the agent's default `127.0.0.1:2000`.
- [`join_trace`](crate::telemetry::join_trace) makes the invocation span the server span of the Lambda trace from its `X-Amzn-Trace-Id`, and marks the first invocation of the process with `faas.coldstart`.
- [`record_status`](crate::telemetry::record_status) records an HTTP status as `http.response.status_code`.

The pipelines call `join_trace` and `record_status` themselves.

**Why it exists.** A span exported over HTTPS costs a network round trip on the request path, or a batch that must be flushed before the response, because Lambda freezes the sandbox as soon as the invocation returns, and a background exporter only runs again at the next invocation, if one comes. The agent sits inside the sandbox and forwards traces outside the invocation, so a local datagram costs microseconds: no connection, no TLS, nothing to flush. This is the transport the AWS Distro for OpenTelemetry Lambda layers use: a JSON header line, `T1S`, then base64 of an OTLP `ExportTraceServiceRequest`. Trace ids start with a timestamp, as X-Ray requires.

**How to use it.** Enable `otel` and call [`telemetry::init`](crate::telemetry::init); nothing else is needed. To combine traces with a subscriber of your own, build the provider directly. Here a local socket plays the agent:

```rust
use std::net::UdpSocket;

use davidrs::telemetry::tracer_provider;
use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};

let agent = UdpSocket::bind("127.0.0.1:0")?;
let provider = tracer_provider(&agent.local_addr()?.to_string(), "orders")?;
provider.tracer("orders").start("load-order").end();

let mut datagram = vec![0_u8; 65_535];
let read = agent.recv(&mut datagram)?;
assert!(datagram[..read].starts_with(b"{\"format\":\"json\",\"version\":1}\nT1S"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

**Use cases.**

- A function behind API Gateway whose spans join the caller's trace in the X-Ray service map.
- Seeing which invocations were cold starts, and what they spent their time on.
- Running locally against an X-Ray daemon by setting `AWS_XRAY_DAEMON_ADDRESS`.

**What it does not do.** Every span is exported: the `Sampled` flag of the incoming trace header is not consulted. A span larger than one datagram (64 KB) is lost. There is no HTTPS exporter, no export of logs or metrics, and no propagation to outbound calls: set `X-Amzn-Trace-Id` on requests you send yourself. A span whose level is filtered out is not traced at all.
