//! SDK configuration for a Lambda sandbox, without `aws-config`.
//!
//! Lambda hands the role credentials over as environment variables, which is
//! the first step of the SDK's default provider chain. The rest of that chain
//! (profiles, instance metadata, STS) never runs in a Lambda, so it is not
//! compiled in. The TLS client can trust a caller-supplied root bundle instead
//! of parsing the operating system's certificate store at startup.
//!
//! The role credentials Lambda injects are temporary and are refreshed by the
//! runtime over the life of a warm execution environment, so [`sdk_config`]
//! does not snapshot them: its credential provider re-reads the `AWS_*`
//! variables on every resolution. A warm invocation that outlives a credential
//! rotation keeps signing with the current set, not the one Lambda has rotated
//! out of the environment.
//!
//! The [configuration chapter](crate::guide::aws_config) of the guide covers
//! choosing the roots and running locally.

use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::Result as CredsResult;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::future;
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
    /// The store is read and parsed when the client first connects. Use this
    /// when the runtime provides a maintained certificate store.
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
/// optional `AWS_SESSION_TOKEN` from the environment, and re-reads the
/// credentials on every resolution so a warm execution environment picks up a
/// rotated session instead of keeping a stale one. When
/// `AWS_CREDENTIAL_EXPIRATION` is present it is parsed and passed to the SDK,
/// giving the lazy identity cache a real staleness signal; when it is absent
/// the re-read still happens on each resolution. Performs no network I/O.
/// Call it once in `main` and build every service client from the result.
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
    let _ = required_env("AWS_ACCESS_KEY_ID")?;
    let _ = required_env("AWS_SECRET_ACCESS_KEY")?;
    Ok(SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(required_env("AWS_REGION")?))
        .credentials_provider(SharedCredentialsProvider::new(EnvCredentials))
        .http_client(http_client)
        .sleep_impl(SharedAsyncSleep::new(TokioSleep::new()))
        .build())
}

/// Credential provider that re-reads the Lambda execution-role environment
/// variables on every resolution.
///
/// Lambda's role credentials are temporary and are rotated by the runtime over
/// the life of a warm execution environment. A provider that read them once,
/// at cold start, would keep a warm invocation signing with a key Lambda has
/// already rotated out of the environment. This provider reads
/// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and the optional
/// `AWS_SESSION_TOKEN` each time the SDK asks for credentials, and, when
/// `AWS_CREDENTIAL_EXPIRATION` is present, parses it so the SDK's lazy cache
/// has a real staleness signal. An unparsable expiration is dropped rather than
/// failing the resolution: the re-read on the next cache eviction still keeps
/// the signing key current.
#[derive(Debug)]
struct EnvCredentials;

impl ProvideCredentials for EnvCredentials {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::ready(env_credentials_from_environment())
    }
}

/// Re-reads the credential variables the Lambda runtime injects.
fn env_credentials_from_environment() -> CredsResult {
    let access_key_id = std::env::var("AWS_ACCESS_KEY_ID")
        .map_err(|_| CredentialsError::not_loaded("missing AWS_ACCESS_KEY_ID"))?;
    let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY")
        .map_err(|_| CredentialsError::not_loaded("missing AWS_SECRET_ACCESS_KEY"))?;
    let session_token = std::env::var("AWS_SESSION_TOKEN").ok();
    let expires_after = std::env::var("AWS_CREDENTIAL_EXPIRATION")
        .ok()
        .and_then(|raw| parse_credential_expiration(&raw));
    Ok(Credentials::new(
        access_key_id,
        secret_access_key,
        session_token,
        expires_after,
        "lambda-env",
    ))
}

/// Parses `AWS_CREDENTIAL_EXPIRATION` into an instant, the way the SDK's own
/// environment provider does.
///
/// Accepts the ISO 8601 shapes AWS emits: `YYYY-MM-DDTHH:MM:SSZ`, an optional
/// fractional-second part, and a `Z`, `±HH:MM`, `±HHMM` or `±HH` offset. Falls
/// back to [`None`] on any deviation, so a malformed value cannot block
/// credential resolution; the next re-read still returns a usable key.
fn parse_credential_expiration(value: &str) -> Option<std::time::SystemTime> {
    use std::time::{Duration, UNIX_EPOCH};

    let value = value.trim();
    if value.len() < 20 {
        return None;
    }
    let bytes = value.as_bytes();
    if (bytes[4], bytes[7], bytes[10], bytes[13], bytes[16]) != (b'-', b'-', b'T', b':', b':') {
        return None;
    }
    let year = int_subset::<i32>(value, 0, 4)?;
    let month = int_subset::<u32>(value, 5, 2)?;
    let day = int_subset::<u32>(value, 8, 2)?;
    let hour = int_subset::<u32>(value, 11, 2)?;
    let minute = int_subset::<u32>(value, 14, 2)?;
    let second = int_subset::<u32>(value, 17, 2)?;
    if month == 0 || month > 12 || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_since_epoch(year, month, day);

    let mut tail = &value[19..];
    let mut nanos = 0_u32;
    if let Some(rest) = tail.strip_prefix('.') {
        let end = rest
            .bytes()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        let digits = &rest[..end];
        let take = digits.len().min(9);
        let scaled: u32 = digits[..take].parse().ok()?;
        nanos = scaled.checked_mul(10_u32.pow(9 - take as u32))?;
        tail = &rest[end..];
    }
    let offset_seconds = parse_offset(tail)?;

    let local_seconds =
        days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);
    let utc_seconds = local_seconds - offset_seconds;
    if utc_seconds < 0 {
        let magnitude = utc_seconds.unsigned_abs();
        UNIX_EPOCH.checked_sub(Duration::new(magnitude, nanos))
    } else {
        let secs = u64::try_from(utc_seconds).ok()?;
        UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
    }
}

/// Parses `len` ASCII digits from `input` starting at `start` into an integer.
fn int_subset<T>(input: &str, start: usize, len: usize) -> Option<T>
where
    T: std::str::FromStr,
{
    let slice = input.get(start..start + len)?;
    if !slice.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    slice.parse().ok()
}

/// Turns a validated proleptic Gregorian date into days since 1970-01-01.
///
/// Howard Hinnant's `days_from_civil`: a closed form that is correct for any
/// valid date, with no leap-year bookkeeping left to the caller.
fn days_since_epoch(mut year: i32, month: u32, day: u32) -> i64 {
    if month <= 2 {
        year -= 1;
    }
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let adjusted_month: i32 = if month > 2 {
        (month - 3) as i32
    } else {
        (month + 9) as i32
    };
    let doy = (153 * adjusted_month + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

/// The number of days in `month` of `year`, accounting for leap years.
fn days_in_month(year: i32, month: u32) -> u32 {
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Parses the trailing time-zone offset an ISO 8601 timestamp carries.
fn parse_offset(tail: &str) -> Option<i64> {
    let bytes = tail.as_bytes();
    match bytes.first()? {
        b'Z' | b'z' if tail.len() == 1 => Some(0),
        b'+' | b'-' => {
            let sign = if bytes[0] == b'+' { 1_i64 } else { -1_i64 };
            let rest = &tail[1..];
            let (hour, minute) = if rest.len() == 5 && rest.as_bytes()[2] == b':' {
                (
                    int_subset::<u32>(rest, 0, 2)?,
                    int_subset::<u32>(rest, 3, 2)?,
                )
            } else if rest.len() == 4 {
                (
                    int_subset::<u32>(rest, 0, 2)?,
                    int_subset::<u32>(rest, 2, 2)?,
                )
            } else if rest.len() == 2 {
                (int_subset::<u32>(rest, 0, 2)?, 0)
            } else {
                return None;
            };
            if hour > 23 || minute > 59 {
                return None;
            }
            Some(sign * (i64::from(hour) * 3_600 + i64::from(minute) * 60))
        }
        _ => None,
    }
}
