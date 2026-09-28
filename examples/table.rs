//! A paged listing over DynamoDB: a bounded query, a signed page token and a
//! span per call.
//!
//! Uses DynamoDB Local at `http://localhost:8000`, a table named by `TABLE`
//! with `PK` / `SK` keys, and `CURSOR_SECRET` for page tokens. Set dummy AWS
//! credentials for local signing; no AWS account is needed. The request
//! must carry gateway authorizer claims; see `examples/README.md`.

use std::sync::Arc;
use std::time::Duration;

use aws_sdk_dynamodb::types::AttributeValue;
use davidrs::dynamo::{self, CursorSecret, PageLimits};
use davidrs::http::access::{Access, Claims, Grant};
use davidrs::http::{Api, Failure, Json, PlainErrors, Request};
use davidrs::{Context, RuntimeError};
use serde::Deserialize;

/// The state every invocation shares.
struct App {
    ddb: aws_sdk_dynamodb::Client,
    table: String,
    cursors: CursorSecret,
}

/// The query string: the token of the page to read.
#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    next: Option<String>,
}

/// Answers up to 50 orders and the token of the next page, reading at most
/// five pages to do it.
async fn list(
    app: Arc<App>,
    listing: Listing,
    context: Context<Grant<String, ()>>,
) -> Result<Json<serde_json::Value>, Failure> {
    let mut start = listing
        .next
        .as_deref()
        .and_then(|token| dynamo::decode_cursor_signed(token, &app.cursors));
    let deadline = context.deadline().with_margin(Duration::from_millis(250));
    let page = dynamo::query_bounded(PageLimits::new(50, 5), deadline, |resume| {
        app.ddb
            .query()
            .table_name(&app.table)
            .key_condition_expression("PK = :user")
            .expression_attribute_values(
                ":user",
                AttributeValue::S(context.scope().caller().clone()),
            )
            .set_exclusive_start_key(resume.or_else(|| start.take()))
    })
    .await?;
    let complete = page.is_complete();
    let items = page
        .items
        .into_iter()
        .map(|item| dynamo::to_object(item, &["PK", "SK"]))
        .collect::<Result<Vec<_>, _>>()?;
    let next = page
        .next
        .map(|key| dynamo::encode_cursor_signed(key, &app.cursors))
        .transpose()?;
    Ok(Json(
        serde_json::json!({ "items": items, "next": next, "complete": complete }),
    ))
}

#[tokio::main]
async fn main() -> Result<(), RuntimeError> {
    let _telemetry = davidrs::telemetry::init("orders-listing")?;
    let config = davidrs::aws::sdk_config(davidrs::aws::Trust::NativeRoots)?;
    let local = aws_sdk_dynamodb::config::Builder::from(&config)
        .endpoint_url("http://localhost:8000")
        .build();
    let app = Arc::new(App {
        ddb: aws_sdk_dynamodb::Client::from_conf(local),
        table: davidrs::required_env("TABLE")?,
        cursors: CursorSecret::new(davidrs::required_env("CURSOR_SECRET")?.as_bytes()),
    });
    let policy =
        Access::new(|claims: &Claims| claims.subject().map(str::to_owned)).require_caller();
    Api::new("list", policy, PlainErrors)
        .run(
            app,
            |request: &Request<'_>| request.query::<Listing>(),
            list,
        )
        .await
}
