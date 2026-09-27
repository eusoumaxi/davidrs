//! Structural validation of a decoded JSON request.
//!
//! A decoder that returns on the first bad field reports one problem per
//! request. These helpers collect every problem instead and render them as a
//! single `400`, which is what a form needs to highlight all of its invalid
//! inputs at once.
//!
//! Issues come in two strengths, because a rule that reads several fields is
//! only meaningful once those fields have the right shape:
//!
//! * [`Issues::abort`] — the value is unusable (wrong type, unknown key), so a
//!   cross-field rule over the enclosing object must not run on it.
//! * [`Issues::check`] — the value has the right shape but is out of range
//!   (too short, too large). Validation continues.
//!
//! [`Issues::mark`] and [`Issues::aborted_since`] gate such a rule on the
//! issues its own object produced rather than on the whole request, so one bad
//! field does not silence the rules of a sibling object.
//!
//! String lengths are counted in UTF-16 code units, the length an HTML form
//! or a UTF-16 client measures, and numeric bounds are printed without a
//! trailing `.0` (`8`, not `8.0`).
//!
//! # Examples
//!
//! ```
//! use davidrs::http::schema::{self, Issues, NumberRule};
//!
//! let body: serde_json::Value = serde_json::json!({"name": "", "age": "x"});
//! let mut issues = Issues::new();
//! let object = schema::expect_object(Some(&body), "", &mut issues).expect("object");
//! schema::expect_string_min(object.get("name"), "name", 1, &mut issues);
//! schema::expect_number(object.get("age"), "age", &NumberRule::int(), &mut issues);
//!
//! let failure = issues.into_result("INVALID_REQUEST").expect_err("invalid");
//! assert_eq!(
//!     failure.public_message(),
//!     "name: must be at least 1 character long; age: expected number, got string"
//! );
//! ```

use lambda_http::http::StatusCode;
use serde_json::{Map, Value};

use super::failure::{Failure, FailureKind};

/// Collected validation issues, each rendered as `path: message`.
///
/// The path of a top-level field is its name; a nested one is dotted
/// (`lines.0.quantity`). The root object itself has the empty path. The
/// failure's code is the caller's: this type collects issues, it does not
/// name them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Issues(Vec<(String, bool)>);

impl Issues {
    /// An empty collection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an issue that makes the value unusable.
    ///
    /// Cross-field rules over the enclosing object are skipped once one of
    /// these is present — see [`Issues::aborted_since`].
    pub fn abort(&mut self, path: &str, message: impl Into<String>) {
        self.0.push((format!("{path}: {}", message.into()), true));
    }

    /// Records an issue that leaves the value usable. Validation continues.
    pub fn check(&mut self, path: &str, message: impl Into<String>) {
        self.0.push((format!("{path}: {}", message.into()), false));
    }

    /// Records an issue that belongs to no single field, so it carries no path.
    ///
    /// Use it for a rule about a combination — "these two cannot both be
    /// set" — where naming one of them would point at the wrong input.
    pub fn message(&mut self, message: impl Into<String>) {
        self.0.push((message.into(), false));
    }

    /// A position to measure a later [`Issues::aborted_since`] against.
    #[must_use]
    pub fn mark(&self) -> usize {
        self.0.len()
    }

    /// Whether any issue recorded since `mark` was an [`Issues::abort`].
    ///
    /// Gate a cross-field rule on this so it only reads values that parsed.
    /// A `mark` past the end reads as "nothing since" instead of panicking.
    #[must_use]
    pub fn aborted_since(&self, mark: usize) -> bool {
        self.0[mark.min(self.0.len())..]
            .iter()
            .any(|(_, aborting)| *aborting)
    }

    /// Whether nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `Ok` when empty, else one `400` under `code` listing every issue.
    ///
    /// # Errors
    ///
    /// Returns the `400` whenever an issue was recorded.
    pub fn into_result(self, code: &'static str) -> Result<(), Failure> {
        if self.0.is_empty() {
            return Ok(());
        }
        let message = self
            .0
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("; ");
        Err(Failure::new(StatusCode::BAD_REQUEST, code, message).with_kind(FailureKind::Decode))
    }
}

/// The name a JSON value reports itself as in a message.
#[must_use]
pub fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Records the shape failure every `expect_*` reports, and yields `None`:
/// `is required` for an absent value, `expected <type>, got <type>` for one
/// of the wrong type.
///
/// One wording for all of them, so two messages differ only by the types
/// they name.
fn wrong_type<T>(
    expected: &str,
    value: Option<&Value>,
    path: &str,
    issues: &mut Issues,
) -> Option<T> {
    let message = match value {
        None => "is required".to_owned(),
        Some(value) => format!("expected {expected}, got {}", type_name(value)),
    };
    issues.abort(path, message);
    None
}

/// `1 item`, `2 items`: a count with its noun, singular or plural.
fn counted(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Extends a path with one key. An array index is passed as its digits.
#[must_use]
pub fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// Rejects keys the schema does not declare. An unknown key aborts.
pub fn check_unknown_keys(
    object: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
    issues: &mut Issues,
) {
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    check_unknown_keys_in(&unknown, path, issues);
}

/// Reports already-collected unknown key names, in the order given.
///
/// Use it when the keys did not come from a [`Map`] — query parameters, for
/// instance, arrive unordered and the route decides how to order them.
pub fn check_unknown_keys_in(unknown: &[&str], path: &str, issues: &mut Issues) {
    if unknown.is_empty() {
        return;
    }
    let quoted: Vec<String> = unknown.iter().map(|key| format!("\"{key}\"")).collect();
    let noun = if unknown.len() == 1 {
        "field"
    } else {
        "fields"
    };
    issues.abort(path, format!("unknown {noun} {}", quoted.join(", ")));
}

/// The value as an object, or `None` after recording an aborting issue.
pub fn expect_object<'a>(
    value: Option<&'a Value>,
    path: &str,
    issues: &mut Issues,
) -> Option<&'a Map<String, Value>> {
    match value {
        Some(Value::Object(object)) => Some(object),
        other => wrong_type("object", other, path, issues),
    }
}

/// The value as a string, or `None` after recording an aborting issue.
pub fn expect_string<'a>(
    value: Option<&'a Value>,
    path: &str,
    issues: &mut Issues,
) -> Option<&'a str> {
    match value {
        Some(Value::String(text)) => Some(text),
        other => wrong_type("string", other, path, issues),
    }
}

/// A string of at least `min` UTF-16 code units.
///
/// A too-short string is still returned: the length is a check, not a shape
/// failure, so the rules that read it can still run.
pub fn expect_string_min<'a>(
    value: Option<&'a Value>,
    path: &str,
    min: usize,
    issues: &mut Issues,
) -> Option<&'a str> {
    let text = expect_string(value, path, issues)?;
    if text.encode_utf16().count() < min {
        issues.check(
            path,
            format!("must be at least {} long", counted(min, "character")),
        );
    }
    Some(text)
}

/// The bounds a number is checked against.
///
/// Build it with [`NumberRule::int`], [`NumberRule::int_between`], or
/// struct-update syntax over [`NumberRule::default`].
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct NumberRule {
    /// Reject a fractional value. A shape failure, so it aborts.
    pub int: bool,
    /// Inclusive lower bound.
    pub min: Option<f64>,
    /// Reject zero and below. Takes precedence over `min`.
    pub positive: bool,
    /// Inclusive upper bound.
    pub max: Option<f64>,
}

impl NumberRule {
    /// An integer with no bounds.
    #[must_use]
    pub const fn int() -> Self {
        Self {
            int: true,
            min: None,
            positive: false,
            max: None,
        }
    }

    /// An integer within an inclusive range.
    #[must_use]
    pub const fn int_between(min: f64, max: f64) -> Self {
        Self {
            int: true,
            min: Some(min),
            positive: false,
            max: Some(max),
        }
    }
}

/// The value as a number within `rule`.
///
/// Returns `None` only for a shape failure (not a number, or fractional where
/// an integer is required); an out-of-range value is returned so the rules
/// that read it can still run.
pub fn expect_number(
    value: Option<&Value>,
    path: &str,
    rule: &NumberRule,
    issues: &mut Issues,
) -> Option<f64> {
    let number = match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
        other => return wrong_type("number", other, path, issues),
    };
    check_number(number, path, rule, issues).then_some(number)
}

/// Applies `rule` to a number that is already in hand.
///
/// Use it for a value the route obtained some other way — a coerced query
/// parameter, say — so the bound is worded the same as it is for a body
/// field. Returns `false` once the value is unusable: fractional where an
/// integer is required, or not finite (an integer rule reports a non-finite
/// value as fractional).
pub fn check_number(number: f64, path: &str, rule: &NumberRule, issues: &mut Issues) -> bool {
    if rule.int && number.fract() != 0.0 {
        issues.abort(path, "must be an integer");
        return false;
    }
    if !number.is_finite() {
        issues.abort(path, "must be a finite number");
        return false;
    }
    if rule.positive && number <= 0.0 {
        issues.check(path, "must be greater than 0");
    } else if let Some(min) = rule.min.filter(|min| number < *min) {
        issues.check(path, format!("must be at least {min}"));
    } else if let Some(max) = rule.max.filter(|max| number > *max) {
        issues.check(path, format!("must be at most {max}"));
    }
    true
}

/// The value as a boolean, or `None` after recording an aborting issue.
pub fn expect_bool(value: Option<&Value>, path: &str, issues: &mut Issues) -> Option<bool> {
    match value {
        Some(Value::Bool(flag)) => Some(*flag),
        other => wrong_type("boolean", other, path, issues),
    }
}

/// The value as an array, or `None` after recording an aborting issue.
pub fn expect_array<'a>(
    value: Option<&'a Value>,
    path: &str,
    issues: &mut Issues,
) -> Option<&'a [Value]> {
    match value {
        Some(Value::Array(items)) => Some(items),
        other => wrong_type("array", other, path, issues),
    }
}

/// The length bounds of an array.
///
/// Call it after checking the elements: a caller reads the element problems
/// first, then learns there are too many of them.
pub fn check_array_size(
    len: usize,
    path: &str,
    min: Option<usize>,
    max: Option<usize>,
    issues: &mut Issues,
) {
    if let Some(min) = min.filter(|min| len < *min) {
        issues.check(path, format!("must have at least {}", counted(min, "item")));
    } else if let Some(max) = max.filter(|max| len > *max) {
        issues.check(path, format!("must have at most {}", counted(max, "item")));
    }
}
