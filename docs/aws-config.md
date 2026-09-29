# AWS configuration

Enable `aws`, or enable `dynamo`, `eventbridge`, `secrets` or `queue-visibility`, which enable it for you. Call [`sdk_config`](crate::aws::sdk_config) once in `main`, before the loop, and pass the result to every `Client::new`. A missing `AWS_REGION` or access key fails that call, and the error names the variable, instead of failing the first request with a signing error.

[`aws::sdk_config`](crate::aws::sdk_config) reads only what Lambda puts in the environment: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` when present, and `AWS_REGION`. It does not run the `aws-config` credential chain (profiles, SSO, IMDS, web identity). The environment provider normally resolves Lambda credentials first; the remaining providers are not compiled into this helper. Like that provider, it reads the credentials again each time the SDK resolves them rather than keeping the first read. The HTTP client uses rustls, and [`Trust`](crate::aws::Trust) chooses which root certificates it accepts. `sdk_config` itself performs no network I/O.

## Why it exists

The usual way to configure the SDK is the `aws-config` crate and its default provider chain. In a Lambda, only the first link of that chain ever answers: the environment variables Lambda sets. The rest (profile files, SSO, container and instance metadata, web identity) is code the function carries and never runs, and it adds to the binary and to the dependency tree. `sdk_config` reads the environment and nothing else.

Two smaller mistakes are prevented on the way:

- **A late configuration error.** A missing variable fails `sdk_config` in `main`, with an error naming the variable, instead of failing the first request with a signing error.
- **A slow start.** Parsing the operating system's certificate store costs time on every cold start. `Trust::Pem` trusts exactly the roots you ship instead.

## How to use it

Call it once in `main`, before the loop starts, and build every client from the result. The `dynamo`, `eventbridge`, `secrets` and `queue-visibility` features each add the SDK crate of their service.

```rust,no_run
use davidrs::aws::{sdk_config, Trust};

let config = sdk_config(Trust::NativeRoots)?;
let region = config.region().map(|region| region.as_ref().to_owned());
# let _ = region;
# Ok::<(), davidrs::RuntimeError>(())
```

Then `aws_sdk_dynamodb::Client::new(&config)`, and the same for every other service the function calls. The DynamoDB chapter shows a complete `main`.

### Choosing the roots

[`Trust::NativeRoots`](crate::aws::Trust::NativeRoots) reads the operating system's store when the client first connects. Use it when the runtime provides a maintained certificate store.

[`Trust::Pem`](crate::aws::Trust::Pem) trusts only the certificates in a PEM bundle, usually compiled in with `include_bytes!`. Most AWS endpoints chain to the Amazon Trust Services roots, so those are the roots to start from. The trade is explicit: if an endpoint ever chains to a root outside the bundle, its calls fail until the bundle is updated and redeployed. The bundle is only parsed when the client first connects, and a bundle with no valid certificate makes the SDK panic there, so make one real call after changing it.

### Local runs

Outside Lambda, put the same variables in the environment yourself. With the AWS CLI:

```text
eval "$(aws configure export-credentials --format env)"
export AWS_REGION=eu-west-1
```

To point a client at a local endpoint, such as DynamoDB Local, override it on the service configuration: `aws_sdk_dynamodb::config::Builder::from(&config).endpoint_url("http://localhost:8000").build()`. Tests that should not touch the network at all can build clients with an in-process HTTP client instead of calling `sdk_config`; this crate's own tests of its AWS helpers do exactly that.

## Use cases

- The `main` of any function that calls DynamoDB, EventBridge, Secrets Manager or SQS.
- A latency-sensitive function that pins its roots to shorten cold starts.
- A function whose deployment should fail fast when its environment is incomplete.

## What it does not do

- **No other credential sources.** No profiles, SSO, instance metadata or role assumption. Outside Lambda, export the variables.
- **No other settings from the environment.** Endpoint overrides, retry modes and timeouts are not read from variables; set them on the service configuration.
- **No clients.** Which services a function calls is its own decision.

## If the client cannot sign or cannot connect

| What you see | What it usually means | What to change |
| --- | --- | --- |
| `main` fails and the error names `AWS_REGION`, `AWS_ACCESS_KEY_ID` or `AWS_SECRET_ACCESS_KEY` | The variable Lambda normally injects is missing | Lambda supplies these reserved variables; check the runtime and execution role rather than setting access keys on the function. For local tests use dummy credentials and a local endpoint. For an intentional AWS call, export temporary credentials with `aws configure export-credentials --format env` and set `AWS_REGION`. |
| It works on your laptop through a profile, and fails in Lambda with no credentials | The deployed runtime or credential source differs from the local profile | Standard Lambda supplies execution-role credentials in its environment. Confirm the runtime and execution role. `sdk_config` intentionally supports only that environment; use the native SDK configuration for other credential sources. |
| The first call to an AWS endpoint fails TLS after you switched to [`Trust::Pem`](crate::aws::Trust::Pem) | The bundle does not contain the root that endpoint chains to | Start from the Amazon Trust Services roots. A bundle with no valid certificate panics on first connect, so make one real call after changing it. |
| Calls go to AWS in a test that should stay on your machine | The client was built with `sdk_config` and no endpoint override | Point the service configuration at `http://localhost:8000`, or build the test client with the SDK's in-process HTTP client and do not call `sdk_config`. |
