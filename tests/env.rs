//! The environment readers: [`required_env`], [`optional_env`], [`list_env`].
//!
//! Setting a variable while another thread reads the environment is a data
//! race, so every test holds [`ENVIRONMENT`] while it runs.

mod support;

use std::sync::{Mutex, MutexGuard, PoisonError};

use davidrs::{RuntimeError, list_env, optional_env, required_env};

/// Serializes the tests of this file.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

/// Sets or clears `name` while holding the lock the other tests wait on.
fn set(name: &str, value: Option<&str>) -> MutexGuard<'static, ()> {
    let guard = ENVIRONMENT.lock().unwrap_or_else(PoisonError::into_inner);
    match value {
        Some(value) => support::env::set(name, value),
        None => support::env::remove(name),
    }
    guard
}

#[test]
fn a_required_variable_is_read_as_set() {
    let _guard = set("DAVIDRS_TEST_TABLE", Some(" orders "));
    assert_eq!(required_env("DAVIDRS_TEST_TABLE").expect("set"), " orders ");
}

#[test]
fn a_missing_required_variable_is_a_configuration_error_naming_it() {
    let _guard = set("DAVIDRS_TEST_MISSING", None);
    let error = required_env("DAVIDRS_TEST_MISSING").expect_err("unset");
    assert!(matches!(
        &error,
        RuntimeError::Configuration(message)
            if message == "missing environment variable DAVIDRS_TEST_MISSING"
    ));
}

#[test]
fn an_optional_variable_is_none_when_unset_empty_or_blank() {
    for value in [None, Some(""), Some("  ")] {
        let _guard = set("DAVIDRS_TEST_OPTIONAL", value);
        assert_eq!(optional_env("DAVIDRS_TEST_OPTIONAL"), None, "{value:?}");
    }
    let _guard = set("DAVIDRS_TEST_OPTIONAL", Some("https://example.com"));
    assert_eq!(
        optional_env("DAVIDRS_TEST_OPTIONAL").as_deref(),
        Some("https://example.com")
    );
}

/// `"https://a.example.com, https://b.example.com"` must allow the second
/// origin, not `" https://b.example.com"`.
#[test]
fn a_list_is_trimmed_and_drops_empty_entries() {
    let _guard = set(
        "DAVIDRS_TEST_ORIGINS",
        Some("https://a.example.com, https://b.example.com ,, "),
    );
    assert_eq!(
        list_env("DAVIDRS_TEST_ORIGINS").expect("set"),
        ["https://a.example.com", "https://b.example.com"]
    );
}

#[test]
fn a_missing_list_is_a_configuration_error() {
    let _guard = set("DAVIDRS_TEST_NO_LIST", None);
    assert!(matches!(
        list_env("DAVIDRS_TEST_NO_LIST"),
        Err(RuntimeError::Configuration(_))
    ));
}
