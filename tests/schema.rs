//! Structural validation with [`davidrs::http::schema`]: every issue of a
//! request collected into one `400`, worded the same way whichever field it
//! is about.
#![cfg(feature = "http")]

use davidrs::http::schema::{
    check_array_size, check_number, check_unknown_keys, check_unknown_keys_in, expect_array,
    expect_bool, expect_number, expect_object, expect_string, expect_string_min, join, type_name,
    Issues, NumberRule,
};
use davidrs::http::{FailureKind, StatusCode};
use serde_json::{json, Value};

/// The rendered message of what `issues` collected.
fn message(issues: Issues) -> String {
    issues
        .into_error("INVALID_REQUEST")
        .expect_err("invalid")
        .public_message()
        .to_owned()
}

#[test]
fn nothing_recorded_is_ok() {
    let issues = Issues::new();
    assert!(issues.is_empty());
    assert!(issues.into_error("INVALID_REQUEST").is_ok());
}

#[test]
fn every_issue_is_reported_in_one_400_not_only_the_first() {
    let body = json!({"name": "", "quantity": 200});
    let mut issues = Issues::new();
    let object = expect_object(Some(&body), "", &mut issues).expect("object");
    expect_string_min(object.get("name"), "name", 3, &mut issues);
    expect_number(
        object.get("quantity"),
        "quantity",
        &NumberRule::int_between(1.0, 100.0),
        &mut issues,
    );
    assert!(!issues.is_empty());
    let failure = issues.into_error("INVALID_REQUEST").expect_err("invalid");
    assert_eq!(failure.status(), StatusCode::BAD_REQUEST);
    assert_eq!(failure.code(), "INVALID_REQUEST");
    assert_eq!(failure.kind(), FailureKind::Decode);
    assert_eq!(
        failure.public_message(),
        "name: must be at least 3 characters long; \
         quantity: must be at most 100"
    );
}

/// The rule reads two fields, so it must not run when one of them is the
/// wrong type: it would compare against a value that is not there.
#[test]
fn an_abort_gates_a_cross_field_rule_and_a_check_does_not() {
    for (until, rule_ran) in [(json!("2026-01-02"), true), (json!(7), false)] {
        let mut issues = Issues::new();
        let mark = issues.mark();
        let from = json!("2026-03-04");
        let from = expect_string(Some(&from), "from", &mut issues);
        let until = expect_string(Some(&until), "until", &mut issues);
        if !issues.aborted_since(mark) {
            assert!(from.is_some() && until.is_some());
            issues.check("until", "must be after from");
        }
        assert_eq!(message(issues).contains("must be after from"), rule_ran);
    }
}

#[test]
fn a_mark_only_sees_issues_recorded_after_it() {
    let mut issues = Issues::new();
    issues.abort("customer", "unusable");
    let mark = issues.mark();
    issues.check("lines", "too many");
    assert!(issues.aborted_since(0));
    assert!(!issues.aborted_since(mark));
    assert!(
        !issues.aborted_since(mark + 10),
        "a mark past the end is empty"
    );
}

#[test]
fn an_issue_about_a_combination_carries_no_path() {
    let mut issues = Issues::new();
    issues.message("Set either email or phone, not both");
    assert!(!issues.aborted_since(0));
    assert_eq!(message(issues), "Set either email or phone, not both");
}

#[test]
fn a_shape_failure_names_what_was_expected_and_what_arrived() {
    let mut issues = Issues::new();
    assert!(expect_object(Some(&json!([])), "customer", &mut issues).is_none());
    assert!(expect_string(Some(&json!(1)), "name", &mut issues).is_none());
    assert!(expect_bool(Some(&json!("yes")), "gift", &mut issues).is_none());
    assert!(expect_array(Some(&json!({})), "lines", &mut issues).is_none());
    assert!(expect_number(
        Some(&json!(true)),
        "quantity",
        &NumberRule::default(),
        &mut issues
    )
    .is_none());
    assert!(issues.aborted_since(0));
    assert_eq!(
        message(issues),
        "customer: expected object, got array; \
         name: expected string, got number; \
         gift: expected boolean, got string; \
         lines: expected array, got object; \
         quantity: expected number, got boolean"
    );
}

#[test]
fn a_well_shaped_value_is_returned() {
    let mut issues = Issues::new();
    let body = json!({"gift": true, "lines": [1, 2], "name": "Ada"});
    assert_eq!(
        expect_bool(body.get("gift"), "gift", &mut issues),
        Some(true)
    );
    assert_eq!(
        expect_array(body.get("lines"), "lines", &mut issues).map(<[Value]>::len),
        Some(2)
    );
    assert_eq!(
        expect_string(body.get("name"), "name", &mut issues),
        Some("Ada")
    );
    assert!(issues.is_empty());
}

#[test]
fn a_missing_value_is_required_and_an_explicit_null_is_a_type() {
    let mut issues = Issues::new();
    expect_string(None, "name", &mut issues);
    expect_string(Some(&Value::Null), "note", &mut issues);
    assert_eq!(
        message(issues),
        "name: is required; \
         note: expected string, got null"
    );
}

#[test]
fn every_json_type_is_named_in_messages() {
    let names: Vec<&str> = [
        json!(null),
        json!(true),
        json!(1.5),
        json!("a"),
        json!([]),
        json!({}),
    ]
    .iter()
    .map(type_name)
    .collect();
    assert_eq!(
        names,
        ["null", "boolean", "number", "string", "array", "object"]
    );
}

#[test]
fn a_nested_path_is_dotted_and_the_root_is_empty() {
    assert_eq!(join("", "lines"), "lines");
    assert_eq!(join("lines", "0"), "lines.0");
    assert_eq!(join(&join("lines", "0"), "quantity"), "lines.0.quantity");
}

/// Two code units in UTF-16, one `char` in Rust: an HTML form measuring the
/// same string reports 2, so the bound must agree.
#[test]
fn a_string_length_counts_utf16_code_units() {
    let mut issues = Issues::new();
    assert_eq!(
        expect_string_min(Some(&json!("🌍")), "name", 2, &mut issues),
        Some("🌍")
    );
    assert!(issues.is_empty());
}

#[test]
fn a_too_short_string_is_still_returned_and_does_not_abort() {
    let mut issues = Issues::new();
    assert_eq!(
        expect_string_min(Some(&json!("")), "name", 1, &mut issues),
        Some("")
    );
    assert!(!issues.aborted_since(0));
    assert_eq!(message(issues), "name: must be at least 1 character long");
}

#[test]
fn a_bound_is_printed_without_a_trailing_zero() {
    let mut issues = Issues::new();
    let at_least_eight = NumberRule {
        min: Some(8.0),
        ..NumberRule::default()
    };
    let at_most_half = NumberRule {
        max: Some(0.5),
        ..NumberRule::default()
    };
    expect_number(Some(&json!(0)), "n", &at_least_eight, &mut issues);
    expect_number(Some(&json!(9)), "m", &at_most_half, &mut issues);
    assert_eq!(
        message(issues),
        "n: must be at least 8; m: must be at most 0.5"
    );
}

#[test]
fn an_out_of_range_number_is_still_returned_and_does_not_abort() {
    let mut issues = Issues::new();
    let rule = NumberRule::int_between(1.0, 10.0);
    assert_eq!(
        expect_number(Some(&json!(0)), "quantity", &rule, &mut issues),
        Some(0.0)
    );
    assert_eq!(
        expect_number(Some(&json!(5)), "quantity", &rule, &mut issues),
        Some(5.0)
    );
    assert!(!issues.aborted_since(0));
    assert_eq!(message(issues), "quantity: must be at least 1");
}

#[test]
fn positive_rejects_zero_and_takes_precedence_over_min() {
    let rule = NumberRule {
        positive: true,
        min: Some(5.0),
        ..NumberRule::default()
    };
    let mut issues = Issues::new();
    assert!(check_number(0.0, "price", &rule, &mut issues));
    assert!(check_number(0.5, "price", &rule, &mut issues));
    assert_eq!(
        message(issues),
        "price: must be greater than 0; \
         price: must be at least 5"
    );
}

#[test]
fn a_fraction_where_an_integer_is_required_aborts() {
    let mut issues = Issues::new();
    assert_eq!(
        expect_number(
            Some(&json!(1.5)),
            "quantity",
            &NumberRule::int(),
            &mut issues
        ),
        None
    );
    assert!(issues.aborted_since(0));
    assert_eq!(message(issues), "quantity: must be an integer");
}

#[test]
fn a_non_finite_number_aborts() {
    for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut issues = Issues::new();
        assert!(!check_number(
            number,
            "ratio",
            &NumberRule::default(),
            &mut issues
        ));
        assert_eq!(message(issues), "ratio: must be a finite number");
    }
    let mut issues = Issues::new();
    assert!(!check_number(
        f64::INFINITY,
        "count",
        &NumberRule::int(),
        &mut issues
    ));
    assert_eq!(message(issues), "count: must be an integer");
}

#[test]
fn unknown_keys_are_listed_together_and_abort() {
    let body = json!({"sku": 1, "colour": 2, "size": 3});
    let mut issues = Issues::new();
    let object = expect_object(Some(&body), "", &mut issues).expect("object");
    check_unknown_keys(object, &["sku"], "", &mut issues);
    assert!(issues.aborted_since(0));
    assert_eq!(message(issues), ": unknown fields \"colour\", \"size\"");
}

#[test]
fn unknown_keys_from_elsewhere_keep_the_order_given() {
    let mut issues = Issues::new();
    check_unknown_keys_in(&[], "query", &mut issues);
    assert!(issues.is_empty());
    check_unknown_keys_in(&["sort"], "query", &mut issues);
    check_unknown_keys_in(&["z", "a"], "filter", &mut issues);
    assert_eq!(
        message(issues),
        "query: unknown field \"sort\"; filter: unknown fields \"z\", \"a\""
    );
}

#[test]
fn only_declared_keys_record_nothing() {
    let body = json!({"sku": 1});
    let mut issues = Issues::new();
    check_unknown_keys(
        body.as_object().expect("object"),
        &["sku", "size"],
        "",
        &mut issues,
    );
    assert!(issues.is_empty());
}

#[test]
fn an_array_size_is_checked_against_both_bounds() {
    let mut issues = Issues::new();
    check_array_size(0, "lines", Some(1), Some(3), &mut issues);
    check_array_size(4, "lines", Some(1), Some(3), &mut issues);
    check_array_size(2, "lines", Some(1), Some(3), &mut issues);
    check_array_size(9, "tags", None, None, &mut issues);
    assert!(!issues.aborted_since(0));
    assert_eq!(
        message(issues),
        "lines: must have at least 1 item; \
         lines: must have at most 3 items"
    );
}
