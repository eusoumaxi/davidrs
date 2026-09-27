//! Logs through the public API: the level, the global subscriber, invocation
//! spans and timed startup.
//!
//! The tests that read `RUST_LOG` or install the global subscriber hold
//! [`ENV`]; the others record into a subscriber of their own thread.
#![cfg(feature = "logs")]

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use davidrs::telemetry::logs::{self, invocation_span, level_from_env, timed_init};
use davidrs::{Deadline, Invocation};
use tracing::field::Field;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Layered, SubscriberExt as _};
use tracing_subscriber::{Layer, Registry};

/// Serializes the tests that touch process-wide state.
static ENV: Mutex<()> = Mutex::new(());

/// Records every span, field update and event as one line of text.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<String>>>);

impl Recorder {
    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn push(&self, mut line: String, fields: impl FnOnce(&mut dyn tracing::field::Visit)) {
        fields(&mut |field: &Field, value: &dyn fmt::Debug| {
            let _ = write!(line, " {field}={value:?}");
        });
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }
}

impl<S: Subscriber> Layer<S> for Recorder {
    fn on_new_span(&self, attributes: &Attributes<'_>, _: &Id, _: Context<'_, S>) {
        let name = format!("span {}", attributes.metadata().name());
        self.push(name, |visit| attributes.record(visit));
    }

    fn on_record(&self, _: &Id, values: &Record<'_>, _: Context<'_, S>) {
        self.push("record".to_owned(), |visit| values.record(visit));
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let level = format!("event {}", event.metadata().level());
        self.push(level, |visit| event.record(visit));
    }
}

fn recording() -> (Recorder, Layered<Recorder, Registry>) {
    let recorder = Recorder::default();
    let subscriber = tracing_subscriber::registry().with(recorder.clone());
    (recorder, subscriber)
}

#[test]
fn the_level_is_a_bare_level_in_rust_log_or_info() {
    let _env = ENV.lock().unwrap_or_else(PoisonError::into_inner);
    for (value, expected) in [
        (Some("debug"), LevelFilter::DEBUG),
        (Some("WARN"), LevelFilter::WARN),
        (Some("off"), LevelFilter::OFF),
        (Some("5"), LevelFilter::TRACE),
        (Some(""), LevelFilter::ERROR),
        (Some("info,my_crate=debug"), LevelFilter::INFO),
        (Some("verbose"), LevelFilter::INFO),
        (None, LevelFilter::INFO),
    ] {
        match value {
            Some(value) => std::env::set_var("RUST_LOG", value),
            None => std::env::remove_var("RUST_LOG"),
        }
        assert_eq!(level_from_env(), expected, "{value:?}");
    }
    std::env::remove_var("RUST_LOG");
}

/// One test, because the global subscriber can be installed once per process.
#[test]
fn telemetry_installs_once_and_a_second_install_is_an_error() {
    let _env = ENV.lock().unwrap_or_else(PoisonError::into_inner);
    std::env::remove_var("RUST_LOG");
    let guard = davidrs::telemetry::init("logs-test").expect("first install");
    assert_eq!(
        format!("{guard:?}"),
        format!("Guard {{ traces: {} }}", cfg!(feature = "otel"))
    );
    assert!(davidrs::telemetry::init("logs-test").is_err());
    let error = logs::init().expect_err("already installed");
    assert!(error.to_string().contains("subscriber"), "{error}");
}

#[tokio::test]
async fn startup_logs_its_component_duration_and_outcome_and_returns_the_result() {
    let (recorder, subscriber) = recording();
    let _default = tracing::subscriber::set_default(subscriber);

    let ready = timed_init("store", async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok::<_, String>(7)
    })
    .await;
    let failed = timed_init("secret", async { Err::<(), _>("token=hunter2".to_owned()) }).await;

    assert_eq!(ready, Ok(7));
    assert_eq!(failed, Err("token=hunter2".to_owned()));
    let lines = recorder.lines();
    assert_eq!(lines.len(), 2, "{lines:#?}");
    for (line, component, success) in [
        (&lines[0], "\"store\"", "true"),
        (&lines[1], "\"secret\"", "false"),
    ] {
        assert!(line.starts_with("event INFO"), "{line}");
        assert!(line.contains(" message=initialized"), "{line}");
        assert!(line.contains(&format!(" component={component}")), "{line}");
        assert!(line.contains(&format!(" success={success}")), "{line}");
    }
    let init_ms: u64 = lines[0]
        .split(" init_ms=")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|ms| ms.parse().ok())
        .expect("init_ms");
    assert!(init_ms >= 20, "{init_ms}");
    assert!(!lines[1].contains("hunter2"), "{}", lines[1]);
}

#[test]
fn an_invocation_span_carries_the_operation_and_the_request_id() {
    let (recorder, subscriber) = recording();
    let invocation = Invocation::new("request-1", Deadline::after(Duration::from_secs(5)));
    let span =
        tracing::subscriber::with_default(subscriber, || invocation_span("orders", &invocation));

    assert!(!span.is_disabled());
    let lines = recorder.lines();
    assert_eq!(
        lines[0],
        "span lambda.invocation operation=\"orders\" request_id=request-1"
    );
}

/// A binary can build its own subscriber around the same CloudWatch format.
#[test]
fn the_text_layer_composes_into_a_custom_subscriber() {
    let (recorder, subscriber) = recording();
    let subscriber = subscriber.with(logs::text_layer()).with(LevelFilter::WARN);
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(attempt = 2, "retrying");
        tracing::info!("filtered out");
    });
    let lines = recorder.lines();
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].starts_with("event WARN"), "{}", lines[0]);
    assert!(lines[0].contains(" message=retrying"), "{}", lines[0]);
}
