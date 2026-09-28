//! SDK configuration for a Lambda sandbox, without `aws-config`.
//!
//! Lambda hands the role credentials over as environment variables, which is
//! the first step of the SDK's default provider chain. The rest of that chain
//! (profiles, instance metadata, STS) never runs in a Lambda, so it is not
//! compiled in. The TLS client can trust a caller-supplied root bundle instead
//! of parsing the operating system's certificate store at startup.
//!
//! The [configuration chapter](crate::guide::aws_config) of the guide covers
//! choosing the roots and running locally.

use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_smithy_async::rt::sleep::{SharedAsyncSleep, TokioSleep};
use aws_smithy_http_client::tls::rustls_provider::CryptoMode;
use aws_smithy_http_client::tls::{Provider, TlsContext, TrustStore};
use aws_smithy_runtime_api::client::behavior_version::BehaviorVersion;
use aws_types::SdkConfig;
use aws_types::region::Region;

use crate::{RuntimeError, required_env};

/// Which root certificates the SDK's TLS client trusts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Trust<'a> {
    /// The operating system's certificate store.
    ///
    /// Correct everywhere, and measurably slower to start: the store is read
    /// and parsed when the client first connects.
    NativeRoots,
    /// Exactly the roots in this PEM bundle.
    ///
    /// Pin the roots the services you call chain to. If a service ever chains
    /// to a root outside the bundle, TLS fails until the bundle is updated and
    /// redeployed; that is the trade for the faster start. The bundle is
    /// parsed when the client first connects, and the SDK panics there if it
    /// holds no valid certificate, so exercise one real call before relying
    /// on a new bundle.
    Pem(&'a [u8]),
}

/// Builds SDK configuration from the environment Lambda injects.
///
/// Reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` and the
/// optional `AWS_SESSION_TOKEN`. Performs no network I/O. Call it once in
/// `main` and build every service client from the result.
///
/// The configuration carries a Tokio timer, which the SDK's retries, timeouts
/// and stalled-stream protection need: without one, building a client
/// panics.
///
/// # Errors
///
/// Returns [`RuntimeError::Configuration`] naming the variable when a
/// required one is absent, and another [`RuntimeError`] when the TLS context
/// cannot be built.
///
/// # Examples
///
/// ```no_run
/// # fn main() -> Result<(), davidrs::RuntimeError> {
/// use davidrs::aws::{sdk_config, Trust};
///
/// let config = sdk_config(Trust::NativeRoots)?;
/// assert!(config.region().is_some());
/// # Ok(())
/// # }
/// ```
pub fn sdk_config(trust: Trust<'_>) -> Result<SdkConfig, RuntimeError> {
    let store = match trust {
        Trust::NativeRoots => TrustStore::empty().with_native_roots(true),
        Trust::Pem(bundle) => TrustStore::empty()
            .with_native_roots(false)
            .with_pem_certificate(bundle.to_vec()),
    };
    let context = TlsContext::builder()
        .with_trust_store(store)
        .build()
        .map_err(|error| RuntimeError::other("building the SDK TLS context", error))?;
    let http_client = aws_smithy_http_client::Builder::new()
        .tls_provider(Provider::Rustls(CryptoMode::Ring))
        .tls_context(context)
        .build_https();
    let credentials = Credentials::new(
        required_env("AWS_ACCESS_KEY_ID")?,
        required_env("AWS_SECRET_ACCESS_KEY")?,
        std::env::var("AWS_SESSION_TOKEN").ok(),
        None,
        "lambda-env",
    );
    Ok(SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(required_env("AWS_REGION")?))
        .credentials_provider(SharedCredentialsProvider::new(credentials))
        .http_client(http_client)
        .sleep_impl(SharedAsyncSleep::new(TokioSleep::new()))
        .build())
}
