# Examples

One runnable program per trigger. Each serves the local Lambda runtime; none
is a deployment. Compile one with its features:

```bash
cargo check --locked --example http --features http
cargo lambda watch --example http --features http
```

| Example | Features | Shows |
| --- | --- | --- |
| `http` | `http` | typed JSON input and output for one handler |
| `validated` | `validate,problem` | a Garde-validated body and RFC 9457 errors |
| `custom_policy` | `http` | a custom policy and error renderer |
| `stream_http` | `http-stream,logs` | JSON or server-sent events from one handler, CORS for one origin |
| `streaming` | `streaming` | a bounded producer owned by the response |
| `queue` | `queue,logs` | a typed SQS payload and explicit delete or retry |
| `event` | `event,logs` | a typed EventBridge event |
| `schedule` | `schedule,logs` | a typed scheduled payload |
| `mcp` | `mcp-openapi` | an MCP server with one hand-written tool and tools from an OpenAPI document |
| `table` | `http,dynamo,logs` | a paged listing: bounded query, page token, spans |

Only `table` needs AWS: a table named by `TABLE` with `PK`/`SK` keys and the
credentials Lambda injects. The others run without an account.
