# Validation

Enable `http`. [`http::schema`](crate::http::schema) checks a decoded JSON value and collects every problem into one `400`, so a form can mark every invalid field at once. It is ordinary code, not a derive.

If the body already deserializes into a struct and you only need Garde rules (`length`, `range`, `dive`), use [`Request::validated_json`](crate::http::Request::validated_json) with the `validate` feature, described in the [HTTP chapter](crate::guide::http). That failure says validation failed and does not name the field. Use this module when the client needs one message such as `name: must be at least 1 character long; quantity: must be at most 100`.

## What it is

[`http::schema`](crate::http::schema) checks the structure of a decoded JSON request and collects every problem in one [`Issues`](crate::http::schema::Issues) value. [`into_result`](crate::http::schema::Issues::into_result) is `Ok` when nothing was recorded, and otherwise one `400` whose message lists every issue as `path: message`, separated by `; `.

## Why it exists

A decoder that stops at the first bad field costs the client one request per mistake, and a form can highlight only one invalid input at a time. A rule that compares two fields has the opposite problem: run on a value that did not parse, it reports nonsense about a field the client got right. `Issues` reports everything at once and still lets such a rule know whether its inputs are usable.

## How to use it

Read each field with an `expect_*` function, which returns the value or `None` after recording why not:

```rust
use davidrs::http::schema::{self, Issues, NumberRule};

let body = serde_json::json!({
    "name": "",
    "quantity": 200,
    "gift": "yes",
    "colour": "red"
});
let mut issues = Issues::new();
if let Some(order) = schema::expect_object(Some(&body), "", &mut issues) {
    schema::check_unknown_keys(order, &["name", "quantity", "gift"], "", &mut issues);
    schema::expect_string_min(order.get("name"), "name", 1, &mut issues);
    schema::expect_number(
        order.get("quantity"),
        "quantity",
        &NumberRule::int_between(1.0, 100.0),
        &mut issues,
    );
    schema::expect_bool(order.get("gift"), "gift", &mut issues);
}

let failure = issues.into_result("INVALID_ORDER").expect_err("invalid");
assert_eq!(failure.status().as_u16(), 400);
assert_eq!(
    failure.public_message(),
    ": unknown field \"colour\"; \
     name: must be at least 1 character long; \
     quantity: must be at most 100; \
     gift: expected boolean, got string"
);
```

The error code is the service's: this module collects issues and does not name them. The failure's kind is `Decode`, like any other bad input.

### Aborting and continuing issues

Issues have two strengths:

- [`abort`](crate::http::schema::Issues::abort): the value is unusable. A wrong type, a fractional number where an integer is required, a non-finite number and an unknown key all abort.
- [`check`](crate::http::schema::Issues::check): the value has the right shape but is out of range, such as too short, too large or too many items. The value is still returned, so the rules that read it can still run.

[`mark`](crate::http::schema::Issues::mark) and [`aborted_since`](crate::http::schema::Issues::aborted_since) gate a cross-field rule on the issues its own object produced, so one bad field elsewhere does not silence it:

```rust
use davidrs::http::schema::{self, Issues};

fn check_period(period: &serde_json::Value, issues: &mut Issues) {
    let mark = issues.mark();
    let from = schema::expect_string(period.get("from"), "period.from", issues);
    let until = schema::expect_string(period.get("until"), "period.until", issues);
    if issues.aborted_since(mark) {
        return;
    }
    if let (Some(from), Some(until)) = (from, until) {
        if until <= from {
            issues.check("period.until", "must be after period.from");
        }
    }
}

let mut issues = Issues::new();
check_period(&serde_json::json!({"from": "2026-03-04", "until": 7}), &mut issues);
let message = issues.into_result("INVALID_PERIOD").unwrap_err().public_message().to_owned();
assert_eq!(message, "period.until: expected string, got number");
```

A rule about a combination rather than one field uses [`message`](crate::http::schema::Issues::message), which carries no path.

### Paths, lengths and numbers

- A top-level field's path is its name; a nested one is dotted, built with [`join`](crate::http::schema::join): `lines.0.quantity`. The root object has the empty path.
- A missing value `is required`; an explicit `null` is a value of the wrong type (`expected string, got null`), so a client can tell the two apart.
- String lengths count UTF-16 code units, the length an HTML form or a UTF-16 client measures: `"🌍"` is 2, not 1.
- Numeric bounds print without a trailing `.0`: `must be at least 8`, not `8.0`. [`NumberRule`](crate::http::schema::NumberRule) holds `int`, `min`, `max` and `positive`; [`check_number`](crate::http::schema::check_number) applies one to a number obtained some other way, such as a query parameter, so its message reads the same as a body field's.
- [`check_unknown_keys`](crate::http::schema::check_unknown_keys) lists every undeclared key in one issue; [`check_unknown_keys_in`](crate::http::schema::check_unknown_keys_in) does the same for names collected elsewhere, in the order given.
- [`check_array_size`](crate::http::schema::check_array_size) checks a length after the elements, so a client reads the element problems first.

## Use cases

- A form posts ten fields and gets every invalid one back in one response.
- A body with a date range, where the "after" rule must run only when both dates are strings.
- A strict contract that rejects any key it does not declare.
- A query parameter coerced to a number, validated with the same wording as a body field.

## What it does not do

- It is not a schema language or a derive: each route writes its checks as ordinary code, in the order the messages should appear.
- It does not coerce types: `"5"` is not a number.
- It does not translate or localize messages, and it does not choose the error code.
- Deriving validation on a type that already deserialized is the separate `validate` feature, through `Request::validated_json`.
