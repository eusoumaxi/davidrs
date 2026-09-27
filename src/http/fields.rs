//! Partial responses: a `fields` query naming the parts of a JSON response
//! the client wants.
//!
//! `fields=a,b.c,d.e.f` keeps `a` whole, `b` reduced to its `c`, and `d`
//! reduced to `e` reduced to `f`. A path with a leading `-` removes it from
//! whatever is kept: `fields=a,-a.b.c` is `a` without `a.b.c`, and
//! `fields=-notes` is everything but `notes`. Arrays are transparent: a path
//! into an array applies to every element, so `items.name` on a list keeps
//! the name of each item and `order.lines.price` keeps the price of each
//! line. A key the response does not have is simply absent; nothing is
//! invented. No `fields` means the whole response.
//!
//! The mask is also how a route decides what work to skip, before any data is
//! read:
//!
//! - [`Mask::wants`] says whether a path is wanted, so a lookup that only
//!   feeds unwanted keys is not made at all.
//! - [`Mask::stored`] turns the request into the attributes or columns to
//!   read, so the database returns only those. [`DynamoProjection`] and
//!   [`sql_columns`] render that list for DynamoDB and for SQL.
//! - [`Mask::apply`] trims whatever remains, down to nested paths, before the
//!   response is serialized.
//!
//! # Examples
//!
//! ```
//! use davidrs::http::fields::Mask;
//! use serde_json::json;
//!
//! let mask = Mask::parse(Some("id,lines.price"));
//! let mut order = json!({"id": 7, "note": "gift", "lines": [{"sku": "a", "price": 3}]});
//! mask.apply(&mut order);
//! assert_eq!(order, json!({"id": 7, "lines": [{"price": 3}]}));
//! assert!(!mask.wants("customer"));
//! ```

use std::collections::BTreeMap;

use serde_json::Value;

/// How many dotted segments of one path are read; the rest are dropped.
///
/// The mask is a tree the depth of its longest path, and it is dropped
/// recursively, so a request-sized path of thousands of segments would
/// overflow the stack. Real paths are a few levels deep.
const MAX_DEPTH: usize = 32;

/// A parsed `fields` value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mask {
    children: BTreeMap<String, Mask>,
    /// Keep everything under this node: a path of the request ended here.
    whole: bool,
    /// Remove this node from whatever is kept.
    excluded: bool,
}

impl Mask {
    /// Everything: the response as it is.
    #[must_use]
    pub fn all() -> Self {
        Self {
            whole: true,
            ..Self::default()
        }
    }

    /// Parses `fields`. Whitespace around names is ignored, empty names
    /// dropped; a value that names nothing to keep keeps everything, minus
    /// its exclusions. A path is read to 32 levels; deeper segments are
    /// dropped, so the request can only widen a path, never crash the
    /// function.
    #[must_use]
    pub fn parse(fields: Option<&str>) -> Self {
        let mut root = Self::default();
        let mut any_included = false;
        for path in fields.unwrap_or_default().split(',') {
            let path = path.trim();
            let (excluded, path) = match path.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, path),
            };
            let mut node = &mut root;
            let mut walked = false;
            for name in path
                .split('.')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .take(MAX_DEPTH)
            {
                node = node.children.entry(name.to_owned()).or_default();
                walked = true;
            }
            if !walked {
                continue;
            }
            if excluded {
                node.excluded = true;
            } else {
                node.whole = true;
                any_included = true;
            }
        }
        if !any_included {
            root.whole = true;
        }
        root
    }

    /// Whether the whole response was asked for, with nothing removed.
    #[must_use]
    pub fn is_all(&self) -> bool {
        self.whole && self.children.is_empty()
    }

    /// Whether anything under `path` (dot-separated) is wanted.
    #[must_use]
    pub fn wants(&self, path: &str) -> bool {
        let mut node = self;
        let mut whole = self.whole;
        for name in path.split('.') {
            if node.excluded {
                return false;
            }
            match node.children.get(name) {
                Some(child) => {
                    node = child;
                    whole = whole || child.whole;
                }
                None => return whole,
            }
        }
        !node.excluded
    }

    /// The mask below `name`, for a nested object the route builds itself.
    ///
    /// An excluded `name` gives a mask that keeps nothing.
    #[must_use]
    pub fn child(&self, name: &str) -> Self {
        match self.children.get(name) {
            Some(child) if child.excluded => Self::default(),
            Some(child) => Self {
                whole: child.whole || self.whole,
                ..child.clone()
            },
            None if self.whole => Self::all(),
            None => Self::default(),
        }
    }

    /// The stored attributes or columns a response needs, so the read fetches
    /// nothing else.
    ///
    /// `fields` maps each top-level response field to the attribute or column
    /// it is read from; `always` names what must be read whatever the client
    /// asked for — keys, and anything the handler's own logic reads. The
    /// result lists `always` first, then the wanted fields in `fields` order,
    /// without repeats. `None` means every field is wanted: read the whole
    /// record rather than a projection that names all of it.
    ///
    /// A response field missing from `fields` cannot be selected, so the
    /// client can only ever narrow the read, never widen it.
    ///
    /// ```
    /// use davidrs::http::fields::Mask;
    ///
    /// const FIELDS: &[(&str, &str)] =
    ///     &[("id", "id"), ("total", "total_cents"), ("customer", "customer_id"), ("notes", "notes")];
    ///
    /// let mask = Mask::parse(Some("total,-notes"));
    /// assert_eq!(mask.stored(FIELDS, &["id"]), Some(vec!["id", "total_cents"]));
    /// assert_eq!(Mask::parse(Some("-notes")).stored(FIELDS, &[]), Some(vec!["id", "total_cents", "customer_id"]));
    /// assert_eq!(Mask::all().stored(FIELDS, &["id"]), None);
    /// ```
    #[must_use]
    pub fn stored<'a>(
        &self,
        fields: &[(&str, &'a str)],
        always: &[&'a str],
    ) -> Option<Vec<&'a str>> {
        let wanted: Vec<&'a str> = fields
            .iter()
            .filter(|(field, _)| self.wants(field))
            .map(|(_, stored)| *stored)
            .collect();
        if wanted.len() == fields.len() {
            return None;
        }
        let mut selected: Vec<&'a str> = Vec::with_capacity(always.len() + wanted.len());
        for name in always.iter().copied().chain(wanted) {
            if !selected.contains(&name) {
                selected.push(name);
            }
        }
        Some(selected)
    }

    /// Reduces `value` in place to what the mask keeps.
    pub fn apply(&self, value: &mut Value) {
        self.apply_with(value, self.whole);
    }

    fn apply_with(&self, value: &mut Value, whole: bool) {
        match value {
            Value::Object(object) => {
                object.retain(|name, _| match self.children.get(name) {
                    Some(child) => !child.excluded,
                    None => whole,
                });
                for (name, child_value) in object.iter_mut() {
                    if let Some(child) = self.children.get(name) {
                        child.apply_with(child_value, whole || child.whole);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.apply_with(item, whole);
                }
            }
            _ => {}
        }
    }
}

/// A DynamoDB `ProjectionExpression` with its `ExpressionAttributeNames`.
///
/// Every name goes through a placeholder (`#p0`, `#p1`, …), because hundreds
/// of ordinary words — `name`, `status`, `date`, `count` — are reserved in
/// DynamoDB expressions and fail the call when written bare. Dotted paths
/// reach into maps: `customer.email` becomes `#p0.#p1`.
///
/// DynamoDB still charges read capacity for the whole item; a projection
/// saves the bytes on the wire, the decoding and the function's memory.
///
/// ```
/// use davidrs::http::fields::{DynamoProjection, Mask};
///
/// const FIELDS: &[(&str, &str)] = &[("id", "id"), ("status", "status"), ("customer", "customer")];
/// let mask = Mask::parse(Some("status"));
/// if let Some(attributes) = mask.stored(FIELDS, &["PK", "SK"]) {
///     let projection = DynamoProjection::new(attributes);
///     assert_eq!(projection.expression(), "#p0, #p1, #p2");
///     assert_eq!(projection.names()[2], ("#p2".to_owned(), "status".to_owned()));
/// }
/// ```
///
/// Pass both parts to the SDK builder of `GetItem`, `Query`, `Scan` or
/// `BatchGetItem`:
///
/// ```no_run
/// # #[cfg(feature = "dynamo")]
/// # async fn read(client: aws_sdk_dynamodb::Client, projection: davidrs::http::fields::DynamoProjection) {
/// let mut query = client.query().table_name("orders").projection_expression(projection.expression());
/// for (placeholder, name) in projection.names() {
///     query = query.expression_attribute_names(placeholder, name);
/// }
/// let _page = query.send().await;
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoProjection {
    expression: String,
    names: Vec<(String, String)>,
}

impl DynamoProjection {
    /// A projection of these attribute paths, in order.
    #[must_use]
    pub fn new<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut names: Vec<(String, String)> = Vec::new();
        let mut expression = String::new();
        for path in paths {
            if !expression.is_empty() {
                expression.push_str(", ");
            }
            let placeholders: Vec<String> = path
                .split('.')
                .map(|segment| {
                    if let Some((placeholder, _)) = names.iter().find(|(_, name)| name == segment) {
                        return placeholder.clone();
                    }
                    let placeholder = format!("#p{}", names.len());
                    names.push((placeholder.clone(), segment.to_owned()));
                    placeholder
                })
                .collect();
            expression.push_str(&placeholders.join("."));
        }
        Self { expression, names }
    }

    /// The `ProjectionExpression`.
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// The `ExpressionAttributeNames` it uses, as `(placeholder, name)` pairs.
    #[must_use]
    pub fn names(&self) -> &[(String, String)] {
        &self.names
    }
}

/// An SQL select list: each column quoted as an ANSI identifier (`"total"`),
/// so a column named like a keyword (`order`, `user`) still works.
///
/// Columns must come from the route's own list — [`Mask::stored`] only ever
/// returns names from it — never from the request. Quoting protects keywords,
/// not arbitrary input. MySQL needs `ANSI_QUOTES` for this form.
///
/// ```
/// use davidrs::http::fields::{sql_columns, Mask};
///
/// const FIELDS: &[(&str, &str)] = &[("id", "id"), ("total", "total_cents"), ("user", "user")];
/// let columns = Mask::parse(Some("user")).stored(FIELDS, &["id"]).unwrap_or_else(|| FIELDS.iter().map(|(_, c)| *c).collect());
/// assert_eq!(sql_columns(&columns), r#""id", "user""#);
/// ```
#[must_use]
pub fn sql_columns(columns: &[&str]) -> String {
    columns
        .iter()
        .map(|column| format!("\"{}\"", column.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ")
}
