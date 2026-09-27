//! A paged listing over DynamoDB: a bounded query, a signed page token and a
//! span per call.
//!
//! Needs a table named by `TABLE` with `PK` / `SK` keys, a secret in
//! `CURSOR_SECRET` to sign page tokens with, and the credentials Lambda
//! injects in the environment; run it with Cargo Lambda.

use std::sync::Arc;

use aws_sdk_dynamodb::types::AttributeValue;
use davidrs::dynamo::{self, CursorSecret, PageLimits};
use davidrs::http::{Api, Failure, Json, PlainErrors, Public, Request};
use davidrs::{Context, RuntimeError};
use serde::Deserialize;

/// The state every invocation shares.
struct App {
    ddb: aws_sdk_dynamodb::Client,
    table: String,
    cursors: CursorSecret,
}

/// The query string: whose orders, and the token of the page to read.
#[derive(Deserialize)]
struct Listing {
    user: String,
    #[serde(default)]
    next: Option<String>,
}

/// Answers up to 50 orders and the token of the next page, reading at most
/// five pages to do it.
async fn list(
    app: Arc<App>,
    listing: Listing,
    context: Context<()>,
) -> Result<Json<serde_json::Value>, Failure> {
    let mut start = listing
        .next
        .as_deref()
        .and_then(|token| dynamo::decode_cursor_signed(token, &app.cursors));
    let page = dynamo::query_bounded(PageLimits::new(50, 5), context.deadline(), |resume| {
        app.ddb
            .query()
            .table_name(&app.table)
            .key_condition_expression("PK = :user")
            .expression_attribute_values(":user", AttributeValue::S(listing.user.clone()))
            .set_exclusive_start_key(resume.or_else(|| start.take()))
    })
    .await?;
    let items = page
        .items
        .into_iter()
        .map(|item| dynamo::to_object(item, &["PK", "SK"]))
        .collect::<Result<Vec<_>, _>>()?;
    let next = page
        .next
        .map(|key| dynamo::encode_cursor_signed(key, &app.cursors))
        .transpose()?;
    Ok(Json(serde_json::json!({ "items": items, "next": next })))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders-listing")?;
    let config = davidrs::aws::sdk_config(davidrs::aws::Trust::NativeRoots)?;
    let app = Arc::new(App {
        ddb: aws_sdk_dynamodb::Client::new(&config),
        table: davidrs::required_env("TABLE")?,
        cursors: CursorSecret::new(davidrs::required_env("CURSOR_SECRET")?.as_bytes()),
    });
    Api::new("list", Public, PlainErrors)
        .run(
            app,
            |request: &Request<'_>| request.query::<Listing>(),
            list,
        )
        .await
}
