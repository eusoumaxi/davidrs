# Partial responses

Enable `http`. A list that shows a name and a price should not download every nested object of every row, and it should not invent its own `fields` parser. [`Mask`](crate::http::fields::Mask) is that parser. The route passes it the query value, so the parameter can have another name. When a field is expensive, use all three steps: [`wants`](crate::http::fields::Mask::wants) skips a lookup, [`stored`](crate::http::fields::Mask::stored) reads fewer attributes or columns, and [`apply`](crate::http::fields::Mask::apply) sends exactly what was asked. `apply` alone still reads the whole record and only trims the JSON afterwards.

## What it is

[`Mask`](crate::http::fields::Mask) is a parsed `fields` value: the parts of a JSON response a client asked for. It reduces a built response to those parts with [`apply`](crate::http::fields::Mask::apply), and tells a route which parts it may skip building with [`wants`](crate::http::fields::Mask::wants).

## Why it exists

A list view that shows a name and a price should not download every nested object of every row, and a route should not read a table whose data the client never asked for. Without a shared rule, each route invents its own parameter and parser, and each gets arrays, exclusions or unknown names slightly differently.

## The syntax

- `fields=a,b.c,d.e.f` keeps `a` whole, `b` reduced to its `c`, and `d` reduced to `e` reduced to `f`. Whitespace around names is ignored and empty names are dropped.
- A leading `-` removes a path from whatever is kept: `fields=-notes` is everything but `notes`, and `fields=customer,-customer.email` is `customer` without its `email`.
- Arrays are transparent: a path into an array applies to every element, so `lines.price` keeps the price of each line.
- A name the response does not have is simply absent; nothing is invented and nothing fails.
- No `fields`, or a value that names nothing to keep, means the whole response, minus any exclusions.

## How to use it

Parse once per request, then either apply the mask to the finished value or ask it before doing the work:

```rust
use davidrs::http::fields::Mask;
use serde_json::json;

let mask = Mask::parse(Some("id,lines.price,-lines.price.tax"));

let mut order = json!({
    "id": 7,
    "note": "gift",
    "lines": [
        {"sku": "a", "price": {"amount": 3, "tax": 1}},
        {"sku": "b", "price": {"amount": 5, "tax": 2}}
    ]
});
mask.apply(&mut order);
assert_eq!(
    order,
    json!({"id": 7, "lines": [{"price": {"amount": 3}}, {"price": {"amount": 5}}]})
);

assert!(mask.wants("lines.price.amount"));
assert!(!mask.wants("lines.price.tax"));
assert!(!mask.wants("customer"));
assert!(Mask::parse(None).is_all());
```

`wants` is how a route skips work: a read that only feeds parts the client did not ask for is not made. [`child`](crate::http::fields::Mask::child) is the mask below one key, for a route that builds a nested object in its own function:

```rust
use davidrs::http::fields::Mask;
use davidrs::http::Request;
use serde_json::{json, Value};

/// The mask of the `fields` query parameter.
fn mask_of(request: &Request<'_>) -> Mask {
    let fields = request
        .query_pairs()
        .into_iter()
        .find(|(name, _)| name == "fields")
        .map(|(_, value)| value);
    Mask::parse(fields.as_deref())
}

/// Builds the customer object, reading the address only when it is wanted.
fn customer(mask: &Mask) -> Value {
    let mut customer = json!({"name": "Ada"});
    if mask.wants("address") {
        customer["address"] = json!({"city": "Lisbon"});
    }
    mask.apply(&mut customer);
    customer
}

let mask = Mask::parse(Some("id,customer.name"));
assert_eq!(customer(&mask.child("customer")), json!({"name": "Ada"}));
assert_eq!(customer(&Mask::all().child("customer"))["address"]["city"], "Lisbon");
```

## Pushing the mask down to the database

`apply` trims a record after it was read; [`wants`](crate::http::fields::Mask::wants) skips a lookup that feeds nothing wanted. The third step is to read less in the first place: [`Mask::stored`](crate::http::fields::Mask::stored) turns the request into the attributes or columns the database should return.

The route declares which stored name each top-level response field comes from, and what it always needs (keys, and anything its own logic reads). The client can only narrow that list, never widen it:

```rust
use davidrs::http::fields::{sql_columns, DynamoProjection, Mask};

/// Response field → stored attribute or column.
const ORDER_FIELDS: &[(&str, &str)] = &[
    ("id", "id"),
    ("status", "status"),
    ("total", "total_cents"),
    ("customer", "customer"),
    ("lines", "lines"),
];

let mask = Mask::parse(Some("status,customer.email"));
let stored = mask.stored(ORDER_FIELDS, &["id"]);
assert_eq!(stored, Some(vec!["id", "status", "customer"]));

if let Some(attributes) = &stored {
    let projection = DynamoProjection::new(attributes.iter().copied());
    assert_eq!(projection.expression(), "#p0, #p1, #p2");

    assert_eq!(sql_columns(attributes), r#""id", "status", "customer""#);
}
```

`None` means every field is wanted: read the whole record, as a request without `fields` does.

- **DynamoDB.** [`DynamoProjection`](crate::http::fields::DynamoProjection) gives the `ProjectionExpression` and its `ExpressionAttributeNames` for `GetItem`, `Query`, `Scan` and `BatchGetItem`. Every name goes through a placeholder, because hundreds of ordinary words (`name`, `status`, `date`) are reserved in DynamoDB expressions. DynamoDB still bills read capacity for the whole item: a projection saves the bytes on the wire, the decoding and the function's memory.
- **SQL.** [`sql_columns`](crate::http::fields::sql_columns) renders the select list with quoted identifiers, so a column named `order` or `user` works. The names come from the route's own table, never from the request.
- **Nested paths** (`customer.email`) select their top-level field; `apply` trims inside it afterwards. Run all three steps: `wants` to skip lookups, `stored` to read less, `apply` to send exactly what was asked.
- **Bounded depth.** A path is read to 32 levels and the rest of it dropped, so a request cannot make the mask deeper than the stack can drop. A cut path keeps a little more than it named, never less.

## Use cases

- A list screen asks for `fields=id,name,lines.price` and gets a fraction of the bytes of the full objects.
- A detail route checks `wants("customer")` before calling the customer service, so clients that do not show the customer do not pay for it.
- A client drops a large, rarely used part with `fields=-history`.

## What it does not do

- It has no wildcards (`lines.*`), no array indexes (`lines.0`) and no renaming.
- It does not validate names: a misspelled path keeps nothing, silently, the same as a name the response lacks.
- It does not read the query string itself; the route passes the `fields` value, so the parameter can have another name.
- It does not build queries. `stored` decides what to read; your repository keeps writing its own `Query`, `GetItem` or SQL.
