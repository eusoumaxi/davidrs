//! CloudWatch Embedded Metric Format documents through the public API.
#![cfg(feature = "metrics")]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use davidrs::telemetry::metrics::{MAX_DIMENSIONS, MAX_METRICS};
use davidrs::telemetry::{Metrics, Unit};
use serde_json::{Value, json};

/// The document stamped at `at`, parsed back.
fn render_at(metrics: &Metrics, at: SystemTime) -> Value {
    serde_json::from_str(&metrics.to_json(at).expect("rendered")).expect("json")
}

fn render(metrics: &Metrics) -> Value {
    render_at(
        metrics,
        UNIX_EPOCH + Duration::from_millis(1_700_000_000_123),
    )
}

#[test]
fn a_document_has_the_emf_shape() {
    let document = render(
        &Metrics::new("Shop")
            .dimension("Operation", "checkout")
            .metric("Latency", 12.5, Unit::Milliseconds)
            .property("orderId", "o-1"),
    );
    assert_eq!(
        document,
        json!({
            "_aws": {
                "Timestamp": 1_700_000_000_123_u64,
                "CloudWatchMetrics": [{
                    "Namespace": "Shop",
                    "Dimensions": [["Operation"]],
                    "Metrics": [{"Name": "Latency", "Unit": "Milliseconds"}],
                }],
            },
            "Operation": "checkout",
            "Latency": 12.5,
            "orderId": "o-1",
        })
    );
}

#[test]
fn a_time_before_the_epoch_is_written_as_zero() {
    let document = render_at(&Metrics::new("N"), UNIX_EPOCH - Duration::from_secs(1));
    assert_eq!(document["_aws"]["Timestamp"], 0);
}

#[test]
fn every_unit_is_spelled_as_cloudwatch_names_it() {
    let metrics = [
        (Unit::Count, "Count"),
        (Unit::Milliseconds, "Milliseconds"),
        (Unit::Bytes, "Bytes"),
        (Unit::Percent, "Percent"),
        (Unit::None, "None"),
    ]
    .iter()
    .fold(Metrics::new("N"), |metrics, (unit, name)| {
        metrics.metric(*name, 1.0, *unit)
    });
    let declared = &render(&metrics)["_aws"]["CloudWatchMetrics"][0]["Metrics"];
    for (index, name) in ["Count", "Milliseconds", "Bytes", "Percent", "None"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(declared[index], json!({"Name": name, "Unit": name}));
    }
}

#[test]
fn an_empty_document_has_one_empty_dimension_set() {
    let metrics = Metrics::new("N");
    assert!(metrics.is_empty());
    assert_eq!(metrics.len(), 0);
    let document = render(&metrics);
    assert_eq!(
        document["_aws"]["CloudWatchMetrics"][0]["Dimensions"],
        json!([[]])
    );
    assert_eq!(
        document["_aws"]["CloudWatchMetrics"][0]["Metrics"],
        json!([])
    );
}

/// CloudWatch rejects a document with more than 100 metrics as a whole.
#[test]
fn metrics_past_the_limit_are_dropped_rather_than_rejecting_the_document() {
    let metrics = (0..MAX_METRICS + 20).fold(Metrics::new("N"), |metrics, index| {
        metrics.metric(format!("m{index}"), index as f64, Unit::Count)
    });
    assert_eq!(metrics.len(), MAX_METRICS);
    assert!(!metrics.is_empty());
    let document = render(&metrics);
    let declared = document["_aws"]["CloudWatchMetrics"][0]["Metrics"]
        .as_array()
        .expect("metrics");
    assert_eq!(declared.len(), MAX_METRICS);
    assert_eq!(document["m99"], 99.0);
    assert_eq!(document.get("m100"), None);
}

#[test]
fn dimensions_past_the_limit_are_dropped() {
    let metrics = (0..MAX_DIMENSIONS + 5).fold(Metrics::new("N"), |metrics, index| {
        metrics.dimension(format!("d{index:02}"), "v")
    });
    let document = render(&metrics);
    let set = document["_aws"]["CloudWatchMetrics"][0]["Dimensions"][0]
        .as_array()
        .expect("dimension set");
    assert_eq!(set.len(), MAX_DIMENSIONS);
    assert_eq!(document.get("d30"), None);
}

#[test]
fn recording_a_metric_again_updates_its_value_and_declares_it_once() {
    let metrics =
        Metrics::new("N")
            .metric("m", 1.0, Unit::Count)
            .metric("m", 2.0, Unit::Milliseconds);
    assert_eq!(metrics.len(), 1);
    let document = render(&metrics);
    assert_eq!(document["m"], 2.0);
    assert_eq!(
        document["_aws"]["CloudWatchMetrics"][0]["Metrics"],
        json!([{"Name": "m", "Unit": "Count"}])
    );
}

/// A full document still accepts updates to what it already holds.
#[test]
fn a_full_document_still_updates_its_metrics_and_dimensions() {
    let full = (0..MAX_DIMENSIONS).fold(Metrics::new("N"), |metrics, index| {
        metrics.dimension(format!("d{index:02}"), "old").metric(
            format!("m{index}"),
            0.0,
            Unit::Count,
        )
    });
    let full = (MAX_DIMENSIONS..MAX_METRICS).fold(full, |metrics, index| {
        metrics.metric(format!("m{index}"), 0.0, Unit::Count)
    });
    let document = render(&full.dimension("d00", "new").metric("m0", 7.0, Unit::Count));
    assert_eq!(document["d00"], "new");
    assert_eq!(document["m0"], 7.0);
}

#[test]
fn non_finite_metrics_are_rejected_before_cloudwatch_discards_the_document() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            Metrics::new("N")
                .metric("ratio", value, Unit::None)
                .to_json(UNIX_EPOCH)
                .is_err()
        );
    }
}

#[test]
fn fields_cannot_overwrite_emf_metadata_or_each_other() {
    for metrics in [
        Metrics::new("N").dimension("_aws", "value"),
        Metrics::new("N").metric("_aws", 1.0, Unit::Count),
        Metrics::new("N").property("_aws", "value"),
        Metrics::new("N")
            .dimension("shared", "value")
            .metric("shared", 1.0, Unit::Count),
        Metrics::new("N")
            .dimension("shared", "value")
            .property("shared", "value"),
        Metrics::new("N")
            .metric("shared", 1.0, Unit::Count)
            .property("shared", "value"),
    ] {
        assert!(metrics.to_json(UNIX_EPOCH).is_err());
    }
}

#[test]
fn properties_take_any_json_value() {
    let document = render(
        &Metrics::new("N")
            .property("attempt", 2)
            .property("cached", true)
            .property("tags", json!(["a", "b"])),
    );
    assert_eq!(document["attempt"], 2);
    assert_eq!(document["cached"], true);
    assert_eq!(document["tags"], json!(["a", "b"]));
}
