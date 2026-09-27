//! Reading secrets from AWS Secrets Manager.
//!
//! Two rules hold here instead of in every caller: a secret's *value* never
//! appears in an error, only its name, and a JSON secret is deserialized into
//! a type instead of being passed around as a map.
//!
//! The [secrets chapter](crate::guide::secrets) of the guide shows where to
//! read them.

use crate::RuntimeError;

/// Fetches a secret's string value.
///
/// # Errors
///
/// Returns [`RuntimeError`] when the call fails (the secret does not exist,
/// access is denied) and [`RuntimeError::Configuration`] when the secret holds
/// only a binary value. The error names the secret, never its contents.
pub async fn string(
    client: &aws_sdk_secretsmanager::Client,
    name: &str,
) -> Result<String, RuntimeError> {
    let response = client
        .get_secret_value()
        .secret_id(name)
        .send()
        .await
        .map_err(|error| RuntimeError::other(format!("reading secret {name}"), error))?;
    response
        .secret_string()
        .map(str::to_owned)
        .ok_or_else(|| RuntimeError::Configuration(format!("secret {name} has no string value")))
}

/// Fetches a secret and deserializes it as JSON.
///
/// # Errors
///
/// As [`string`], plus [`RuntimeError::Configuration`] when the value is not
/// the expected shape. That error gives only the line and column: a
/// `serde_json` message can quote the offending input, which here is the
/// secret.
///
/// # Examples
///
/// ```no_run
/// # async fn load(client: aws_sdk_secretsmanager::Client) -> Result<(), davidrs::RuntimeError> {
/// #[derive(serde::Deserialize)]
/// struct Upstream {
///     api_key: String,
/// }
///
/// let upstream: Upstream = davidrs::secrets::json(&client, "example/upstream").await?;
/// # let _ = upstream.api_key;
/// # Ok(())
/// # }
/// ```
pub async fn json<T: serde::de::DeserializeOwned>(
    client: &aws_sdk_secretsmanager::Client,
    name: &str,
) -> Result<T, RuntimeError> {
    let value = string(client, name).await?;
    serde_json::from_str(&value).map_err(|error| {
        RuntimeError::Configuration(format!(
            "secret {name} is not the expected shape (at line {}, column {})",
            error.line(),
            error.column()
        ))
    })
}
