# AWS configuration

[`aws::sdk_config`](crate::aws::sdk_config) builds the `SdkConfig` every AWS SDK client is made from, using what Lambda puts in the function's environment: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` when present, and `AWS_REGION`. Its HTTP client uses rustls, and [`Trust`](crate::aws::Trust) chooses which root certificates it accepts. It performs no network I/O.

## Why it exists

The usual way to configure the SDK is the `aws-config` crate and its default provider chain. In a Lambda, only the first link of that chain ever answers: the environment variables Lambda sets. The rest (profile files, SSO, container and instance metadata, web identity) is code the function carries and never runs, and it adds to the binary and to the dependency tree. `sdk_config` reads the environment and nothing else.

Two smaller mistakes are prevented on the way:

- **A late configuration error.** A missing variable fails `sdk_config` in `main`, with an error naming the variable, instead of failing the first request with a signing error.
- **A slow start.** Parsing the operating system's certificate store costs time on every cold start. `Trust::Pem` trusts exactly the roots you ship instead.

## How to use it

Call it once in `main`, before the loop starts, and build every client from the result. The `dynamo`, `events`, `secrets` and `queue-visibility` features each add the SDK crate of their service.

```rust
use davidrs::aws::{sdk_config, Trust};

# std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
# std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-example");
# std::env::set_var("AWS_REGION", "eu-west-1");
let config = sdk_config(Trust::NativeRoots)?;
assert_eq!(config.region().map(|region| region.as_ref()), Some("eu-west-1"));
# Ok::<(), davidrs::RuntimeError>(())
```

Then `aws_sdk_dynamodb::Client::new(&config)`, and the same for every other service the function calls. The DynamoDB chapter shows a complete `main`.

### Choosing the roots

[`Trust::NativeRoots`](crate::aws::Trust::NativeRoots) reads the operating system's store when the client first connects. It is correct everywhere and the right default.

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
- **No refresh.** The credentials are read once, when `sdk_config` runs.
- **No other settings from the environment.** Endpoint overrides, retry modes and timeouts are not read from variables; set them on the service configuration.
- **No clients.** Which services a function calls is its own decision.
