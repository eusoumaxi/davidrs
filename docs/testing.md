# Testing

This chapter has two parts. The first is how to test a function you wrote, without an AWS account. The second is how this crate verifies itself, which is what `scripts/check.sh` runs.

## Testing your handlers

A handler is an async function of its inputs, so most tests call it directly. For the pipeline around it — decoding, the policy, rendering, headers — call [`Api::handle`](crate::http::Api::handle) with a request built by [`test_support`](crate::test_support) (feature `test-support`, enabled only in `[dev-dependencies]`):

```rust
use std::sync::Arc;

use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request, StatusCode};
use davidrs::{test_support, Context};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct NewItem {
    name: String,
}

#[derive(Serialize)]
struct Created {
    id: String,
}

async fn create(_app: Arc<()>, input: NewItem, _context: Context<()>) -> Result<(StatusCode, Json<Created>), Failure> {
    Ok((StatusCode::CREATED, Json(Created { id: format!("item-{}", input.name) })))
}

# tokio::runtime::Runtime::new().unwrap().block_on(async {
let api = Api::new("create-item", Public, PlainErrors);
let decode = |request: &Request| request.json::<NewItem>();

let created = api
    .handle(Arc::new(()), test_support::post_json("https://example.com/items", r#"{"name":"a"}"#), &decode, &create)
    .await;
assert_eq!(created.status(), StatusCode::CREATED);

let refused = api
    .handle(Arc::new(()), test_support::post_json("https://example.com/items", "not json"), &decode, &create)
    .await;
assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
# });
```

Other triggers have the same seam: [`queue::process`](crate::queue::process) runs a batch without the Lambda loop, and [`StreamApi::handle`](crate::http::stream::StreamApi::handle) runs one streamed invocation. [`test_support::invocation`](crate::test_support::invocation) and [`test_support::expired_invocation`](crate::test_support::expired_invocation) build invocations with a generous or an exhausted budget, so deadline paths are testable too.

`test-support` adds synthetic input only. It contains no way to build an authorized scope or skip a policy: a test-only shortcut that exists in a production build is a vulnerability, not a convenience.

For AWS calls, give the SDK client an in-process HTTP client (the SDK's `test-util` replay and closure clients) instead of an account: the helpers in this crate take the SDK client as an argument, so a test controls every answer, including partial ones.

## How this crate is verified

Everything below runs with one command, and CI runs the same steps:

```bash
scripts/check.sh              # every step, in order
scripts/check.sh coverage     # line coverage, with a floor
```

| Step | What it proves |
| --- | --- |
| `rules` | comments are rustdoc only, and `src/` has no test code |
| `lint` | `rustfmt`, and `clippy -D warnings` with all features and with none, including the documentation lints |
| `test` | every test and doctest with all features |
| `features` | each feature alone compiles cleanly and passes its tests, so no feature silently depends on another |
| `msrv` | everything compiles with the oldest supported Rust, `rust-version` in `Cargo.toml` |
| `docs` | the API reference and this guide build with no warnings, and every link resolves |
| `package` | the crate packages and builds from its own files |
| `spelling` | code, comments and documentation have no known misspellings (`typos`) |
| `workflows` | the GitHub Actions workflows and the shell scripts pass `actionlint` and `shellcheck` |
| `deny` | third-party licences, sources and security advisories |
| `coverage` | line coverage stays above the floor |

The tests themselves follow a few rules, listed in `CONTRIBUTING.md`:

- **Public API only.** Tests live in `tests/`, one file per area, and use the crate the way an application does. A refactor that keeps behaviour keeps every test green.
- **The real loop, end to end.** `tests/lambda_loop.rs` runs every `run` entry point against a local imitation of the Lambda Runtime API, including the metadata prelude of a streamed response.
- **Offline and deterministic.** Remote ends are a local HTTP server (`tests/support/server.rs`) or the SDK's in-process client; no test needs the network or an AWS account.
- **Checked against the specification.** Wire formats are asserted byte for byte against what the other side expects: the SQS partial-batch response, the Runtime API streaming prelude, CloudWatch EMF, X-Ray trace headers, RFC 9457 problem details and the IETF rate-limit fields.
- **Every example runs.** Code in doc comments and in this guide is compiled and executed as a doctest.
