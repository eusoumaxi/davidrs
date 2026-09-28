# Examples

Each file in this directory is a complete function for one trigger or one feature. None of them is a deployment, and none of them is what `cargo lambda watch` serves. Watch serves binary crates. These are Cargo examples.

## Compile one

From the repository root, check the example with the features it needs. The feature list is the whole point of the command: an example that compiles only because `--all-features` is on does not show you the binary you would deploy.

```bash
cargo check --locked --example http --features http
```

## Run one locally

```bash
cargo lambda new --http hello
```

For other triggers, omit `--http` and choose the matching event type. Copy the example into that crate's `src/main.rs`, and add `davidrs` with the features below plus the direct dependencies the example imports (`serde`, `serde_json`, SDK types or tracing). Then:

```bash
cargo lambda watch
```

For an HTTP example, add a route under `[package.metadata.lambda.watch.router]` so a path parameter reaches the decoder. [Getting started](https://eusoumaxi.github.io/davidrs/davidrs/guide/getting_started/index.html) shows the route for `/hello/{name}` and the two ways the emulator differs from Lambda: the deadline is always ten minutes, and CORS headers are added unless you pass `--disable-cors`.

## Which file to copy

| Example | Features | What you should see when it works |
| --- | --- | --- |
| `http` | `http` | `POST` a JSON body `{"name":"..."}`. The response is a greeting. No AWS and no configuration. |
| `validated` | `validate,problem` | A body that breaks a Garde rule is rejected before the handler, as RFC 9457 problem details. |
| `custom_policy` | `http` | A policy you write, and an error renderer you write, instead of `Public` and `PlainErrors`. |
| `stream_http` | `http-stream,logs` | One handler answers JSON or server-sent events, and CORS allows one origin. |
| `streaming` | `streaming` | The response body owns its producer. Dropping the body cancels that producer. |
| `queue` | `queue,logs` | Each SQS message is deleted or retried on its own. The response lists only the failures. |
| `event` | `event,logs` | An EventBridge event is decoded into your type before the handler runs. |
| `schedule` | `schedule,logs` | A scheduled payload is decoded the same way. |
| `mcp` | `mcp-openapi` | An MCP server with one hand-written tool and tools taken from an OpenAPI document. |
| `table` | `http,dynamo,logs` | A paged listing: a bounded query, a page token, and a span per call. |

`table` targets DynamoDB Local at `http://localhost:8000`. Create a local table with string keys `PK` and `SK`, set `TABLE`, a local `CURSOR_SECRET`, `AWS_REGION=us-east-1`, and dummy `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`. No AWS account is needed. An unauthenticated HTTP call returns `401`; tests can supply synthetic gateway authorizer claims. The partition comes from the caller's verified `sub`, never a query parameter. For a deployed function, construct the client with `aws_sdk_dynamodb::Client::new(&config)`, load a signing secret securely, and restrict invocation to the configured gateway authorizer. `mcp` needs `API_URL`, `MCP_URL` and `ISSUER`, and answers only callers that already have API Gateway JWT authorizer claims. The other examples need no configuration and no AWS account.

If `cargo check --example` fails with an unresolved import, the feature in the table is missing from the command. If it succeeds in the workspace and fails when you copy it into its own crate, the workspace was enabling that feature through another package. Check the copied crate on its own.
