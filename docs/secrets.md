# Secrets

Enable `secrets`. The [`secrets`](crate::secrets) module reads one secret from AWS Secrets Manager. [`string`](crate::secrets::string) returns the string value. [`json`](crate::secrets::json) deserializes that value into a type of yours. Both take the client and the secret's name or ARN. Call them in `main`, and store the typed value on `App`. The environment variable holds the name or ARN (`required_env("SIGNING_SECRET")`), never the secret itself. The function's role needs `secretsmanager:GetSecretValue` on that secret and nothing else. A malformed JSON secret reports a line and column. The error names the secret and does not include the value, so it can be logged as it is.

## Why it exists

A secret leaks most often through an error message, not through the code that uses it:

- **Parse errors quote their input.** A `serde_json` error on a malformed secret can print the offending value, and the error is then logged. `json` reports only the line and column where parsing failed.
- **Errors carry what they were given.** Every error here names the secret and never includes its value, so it can be logged as it is.
- **Maps spread further than types.** A secret passed around as a map of strings reaches places it should not and fails late when a field is missing. A typed secret fails once, at startup, and names what is wrong.

## How to use it

Read secrets once, while building the application state in `main`, and keep the typed value. A missing or malformed secret then fails the cold start with a clear error, instead of the first request that needs it.

```rust,no_run
use std::sync::Arc;

use davidrs::aws::{sdk_config, Trust};
use davidrs::{required_env, RuntimeError};

/// The credentials of an upstream service, as stored in the secret.
#[derive(serde::Deserialize)]
struct UpstreamCredentials {
    client_id: String,
    client_secret: String,
}

/// The shared state every invocation receives.
struct App {
    upstream: UpstreamCredentials,
    signing_key: String,
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let config = sdk_config(Trust::NativeRoots)?;
    let client = aws_sdk_secretsmanager::Client::new(&config);
    let app = Arc::new(App {
        upstream: davidrs::secrets::json(&client, &required_env("UPSTREAM_SECRET")?).await?,
        signing_key: davidrs::secrets::string(&client, &required_env("SIGNING_SECRET")?).await?,
    });
    Ok(())
}
```

The name comes from configuration; the value never does. A secret stored as binary only is a [`RuntimeError::Configuration`](crate::RuntimeError::Configuration), as is a JSON secret of the wrong shape. A secret that does not exist, or that the function's role may not read, is the call's error, naming the secret.

## Use cases

- API keys and client credentials for an upstream service, stored as one JSON secret.
- A signing key or webhook secret, stored as a plain string.
- Database credentials, read once at startup and reused by every warm invocation.

## What it does not do

- **No caching or rotation.** Read once in `main`; after a rotation, new execution environments read the new value. A function that must follow rotation without a cold start reads the secret again itself.
- **No binary secrets.** Only the string value is read.
- **No versions or stages.** The current version is read; for another one, call the SDK directly.
- **No redaction elsewhere.** Once you hold the value, keeping it out of logs and out of a failure's message is up to you. `Failure::internal` and `RuntimeError` will happily carry a string you format yourself. Do not format the secret into either.

## If startup fails on a secret

| What you see | What it usually means | What to change |
| --- | --- | --- |
| The cold start fails and the error names the variable, not the secret | `required_env` did not find the name or ARN | Set `SIGNING_SECRET` (or whichever name you chose) to the secret's name or ARN. The value does not belong in the environment. |
| The error names the secret and is an access or not-found error from the SDK | The role cannot `GetSecretValue`, or the name is wrong | Grant that action on that secret only. Confirm the name in the same account and Region as the function. |
| The error is [`RuntimeError::Configuration`](crate::RuntimeError::Configuration) and mentions a line and column | The secret is JSON of the wrong shape, or it is not JSON | Store the fields your struct expects. The message does not contain the secret. |
| The same variant, and the secret is binary | [`string`](crate::secrets::string) and [`json`](crate::secrets::json) read the string value only | Store it as text, or call the SDK yourself for a binary secret. |
| A warm environment keeps the old value after a rotation | The secret was read once in `main` | A new execution environment reads the new value. Read again yourself only if you must rotate without a cold start. |
