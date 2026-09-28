//! Partial responses: parsing a `fields` value into a [`Mask`], applying it
//! to a JSON response, and asking it which work a route can skip.
#![cfg(feature = "http")]

use davidrs::http::fields::{DynamoProjection, Mask, sql_columns};
use serde_json::json;

#[test]
fn a_mask_keeps_named_paths_through_arrays() {
    let mask = Mask::parse(Some("id, lines.sku,lines.tags.label, missing"));
    let mut order = json!({
        "id": 1,
        "note": "gift",
        "lines": [
            { "sku": "a", "price": 3, "tags": [{ "label": "x", "color": "red" }] },
            { "sku": "b", "tags": null }
        ]
    });
    mask.apply(&mut order);
    assert_eq!(
        order,
        json!({ "id": 1, "lines": [
            { "sku": "a", "tags": [{ "label": "x" }] },
            { "sku": "b", "tags": null }
        ] })
    );
}

#[test]
fn an_exclusion_alone_keeps_everything_else() {
    let mask = Mask::parse(Some("-notes,-customer.lines.price"));
    assert!(!mask.is_all());
    let mut order = json!({
        "notes": [],
        "status": "open",
        "customer": { "name": "Ada", "lines": [{ "sku": "a", "price": 2 }] }
    });
    mask.apply(&mut order);
    assert_eq!(
        order,
        json!({
            "status": "open",
            "customer": { "name": "Ada", "lines": [{ "sku": "a" }] }
        })
    );
}

#[test]
fn an_exclusion_removes_from_what_is_kept() {
    let mask = Mask::parse(Some("customer.lines,-customer.lines.payment.refunds"));
    let mut order = json!({
        "status": "open",
        "customer": {
            "name": "Ada",
            "lines": [{
                "sku": "a",
                "payment": { "paid": true, "refunds": [{ "id": "r" }] }
            }]
        }
    });
    mask.apply(&mut order);
    assert_eq!(
        order,
        json!({ "customer": { "lines": [{ "sku": "a", "payment": { "paid": true } }] } })
    );
}

#[test]
fn absent_or_empty_fields_keep_everything() {
    for fields in [None, Some(""), Some(" , ,"), Some("-")] {
        let mask = Mask::parse(fields);
        assert!(mask.is_all(), "{fields:?}");
        assert_eq!(mask, Mask::all());
        let mut value = json!({ "a": { "b": 1 } });
        mask.apply(&mut value);
        assert_eq!(value, json!({ "a": { "b": 1 } }));
    }
}

#[test]
fn scalars_are_left_as_they_are() {
    let mask = Mask::parse(Some("id"));
    let mut value = json!("plain");
    mask.apply(&mut value);
    assert_eq!(value, json!("plain"));
}

#[test]
fn wants_answers_for_whole_partial_and_excluded_paths() {
    let mask = Mask::parse(Some(
        "customer.lines.payment,status,-customer.lines.payment.refunds",
    ));
    assert!(mask.wants("status"));
    assert!(mask.wants("customer"));
    assert!(mask.wants("customer.lines.payment"));
    assert!(mask.wants("customer.lines.payment.paid"));
    assert!(!mask.wants("customer.lines.payment.refunds"));
    assert!(!mask.wants("customer.lines.payment.refunds.id"));
    assert!(!mask.wants("customer.lines.images"));
    assert!(!mask.wants("notes"));
    assert!(Mask::all().wants("anything.at.all"));
    assert!(!Mask::parse(Some("-notes")).wants("notes"));
    assert!(Mask::parse(Some("-notes")).wants("status"));
}

#[test]
fn a_child_mask_is_what_the_parent_keeps_below_one_key() {
    let mask = Mask::parse(Some("customer.lines,status"));
    assert!(mask.child("customer").wants("lines"));
    assert!(!mask.child("customer").wants("name"));
    assert!(mask.child("status").is_all());
    assert!(!mask.child("shipment").wants("carrier"));
    assert!(Mask::parse(Some("-notes")).child("shipment").is_all());

    let mut customer = json!({ "name": "Ada", "lines": [] });
    Mask::parse(Some("-customer.name"))
        .child("customer")
        .apply(&mut customer);
    assert_eq!(customer, json!({ "lines": [] }));
}

#[test]
fn an_excluded_child_keeps_nothing() {
    let child = Mask::parse(Some("-notes")).child("notes");
    assert!(!child.is_all());
    assert!(!child.wants("text"));
    let mut notes = json!({ "text": "call first" });
    child.apply(&mut notes);
    assert_eq!(notes, json!({}));
}

/// Response fields and the attributes or columns they are read from.
const FIELDS: &[(&str, &str)] = &[
    ("id", "id"),
    ("total", "total_cents"),
    ("customer", "customer_id"),
    ("notes", "notes"),
];

#[test]
fn stored_reads_only_what_the_request_wants_plus_what_the_handler_needs() {
    assert_eq!(
        Mask::parse(Some("total,customer.name")).stored(FIELDS, &["id"]),
        Some(vec!["id", "total_cents", "customer_id"]),
        "a nested path needs its top-level field"
    );
    assert_eq!(
        Mask::parse(Some("-notes")).stored(FIELDS, &[]),
        Some(vec!["id", "total_cents", "customer_id"])
    );
    assert_eq!(
        Mask::parse(Some("id,total")).stored(FIELDS, &["id", "PK"]),
        Some(vec!["id", "PK", "total_cents"]),
        "no name is read twice"
    );
}

#[test]
fn stored_is_none_when_everything_is_wanted() {
    assert_eq!(Mask::all().stored(FIELDS, &["id"]), None);
    assert_eq!(Mask::parse(Some("-nothing.here")).stored(FIELDS, &[]), None);
    assert_eq!(
        Mask::parse(Some("id,total,customer,notes")).stored(FIELDS, &[]),
        None
    );
}

#[test]
fn a_request_cannot_widen_the_read_beyond_the_routes_fields() {
    assert_eq!(
        Mask::parse(Some("password_hash")).stored(FIELDS, &["id"]),
        Some(vec!["id"])
    );
}

#[test]
fn a_dynamo_projection_puts_every_name_behind_a_placeholder() {
    let projection = DynamoProjection::new(["PK", "status", "customer.email", "customer.name"]);
    assert_eq!(projection.expression(), "#p0, #p1, #p2.#p3, #p2.#p4");
    assert_eq!(
        projection.names(),
        [
            ("#p0".to_owned(), "PK".to_owned()),
            ("#p1".to_owned(), "status".to_owned()),
            ("#p2".to_owned(), "customer".to_owned()),
            ("#p3".to_owned(), "email".to_owned()),
            ("#p4".to_owned(), "name".to_owned()),
        ]
    );
    assert_eq!(DynamoProjection::new([]).expression(), "");
}

/// The mask is a tree as deep as its longest path, and dropping it recurses:
/// parsed in full, a request-sized path would overflow the stack.
#[test]
fn a_path_deeper_than_the_limit_is_cut_instead_of_crashing() {
    let deep = vec!["a"; 100_000].join(".");
    let mask = Mask::parse(Some(&deep));
    assert!(mask.wants(&vec!["a"; 40].join(".")));
    let mut value = json!({ "a": { "a": { "b": 1 } }, "z": 2 });
    mask.apply(&mut value);
    assert_eq!(value, json!({ "a": { "a": {} } }));
}

#[test]
fn sql_columns_quote_identifiers_so_keywords_work() {
    assert_eq!(
        sql_columns(&["id", "order", "user"]),
        r#""id", "order", "user""#
    );
    assert_eq!(sql_columns(&[r#"odd"name"#]), r#""odd""name""#);
    assert_eq!(sql_columns(&[]), "");
}
