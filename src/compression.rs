//! Gzip with a cap on the decoded size.
//!
//! Calling `read_to_end` on a decoder invites a decompression bomb: a few
//! kilobytes can expand to gigabytes. Every decoding function here caps the
//! **decoded** size,
//! whatever the compressed size, and reports the cap as an error instead of
//! returning a truncated value.

use std::io::Read;

use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;

use crate::RuntimeError;

/// Compresses `data` with gzip at the default level.
///
/// # Errors
///
/// Returns [`RuntimeError`] when the encoder fails, which an in-memory
/// encoder does not do in practice.
pub fn gzip(data: &[u8]) -> Result<Vec<u8>, RuntimeError> {
    use std::io::Write as _;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(data)
        .and_then(|()| encoder.finish())
        .map_err(|error| RuntimeError::other("gzip", error))
}

/// Decompresses gzip data, refusing to produce more than `limit` bytes.
///
/// It decodes at most `limit + 1` bytes: when that extra byte appears, the
/// input expands past the cap and the call fails, instead of returning a
/// prefix that would be mistaken for the whole value.
///
/// # Errors
///
/// Returns [`RuntimeError::LimitExceeded`] when the decoded size would exceed
/// `limit`, or another [`RuntimeError`] when the data is not valid gzip.
///
/// # Examples
///
/// ```
/// use davidrs::compression::{gunzip_bounded, gzip};
///
/// let packed = gzip(&[0_u8; 1024])?;
/// assert_eq!(gunzip_bounded(&packed, 1024)?.len(), 1024);
/// assert!(gunzip_bounded(&packed, 1023).is_err());
/// # Ok::<(), davidrs::RuntimeError>(())
/// ```
pub fn gunzip_bounded(data: &[u8], limit: usize) -> Result<Vec<u8>, RuntimeError> {
    let mut decoded = Vec::new();
    MultiGzDecoder::new(data)
        .take((limit as u64).saturating_add(1))
        .read_to_end(&mut decoded)
        .map_err(|error| RuntimeError::other("gunzip", error))?;
    if decoded.len() > limit {
        return Err(RuntimeError::LimitExceeded {
            kind: "decoded bytes",
            limit: limit as u64,
        });
    }
    Ok(decoded)
}

/// Decompresses gzip data into a UTF-8 string, bounded by `limit` bytes.
///
/// # Errors
///
/// As [`gunzip_bounded`], plus a failure when the result is not UTF-8.
pub fn gunzip_to_string_bounded(data: &[u8], limit: usize) -> Result<String, RuntimeError> {
    let decoded = gunzip_bounded(data, limit)?;
    String::from_utf8(decoded).map_err(|error| RuntimeError::other("gunzip utf-8", error))
}
