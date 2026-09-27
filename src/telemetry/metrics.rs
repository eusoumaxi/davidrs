//! CloudWatch Embedded Metric Format (EMF).
//!
//! EMF turns a log line into metrics, so a Lambda function emits metrics
//! without an SDK call on the request path. What this module adds is the
//! bounds: CloudWatch rejects a document with too many metrics or dimensions,
//! and a rejected document loses every metric in it, silently.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// CloudWatch's limit on metrics in one EMF document.
pub const MAX_METRICS: usize = 100;

/// CloudWatch's limit on dimensions in one dimension set.
pub const MAX_DIMENSIONS: usize = 30;

/// A metric's unit, as CloudWatch names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub enum Unit {
    /// A plain count.
    Count,
    /// Milliseconds.
    Milliseconds,
    /// Bytes.
    Bytes,
    /// A percentage from 0 to 100.
    Percent,
    /// No unit.
    None,
}

/// One EMF document: a namespace, one dimension set, metrics and searchable
/// properties.
///
/// Print [`to_json`](Self::to_json) as one line to standard output and
/// CloudWatch Logs extracts the metrics.
///
/// ```
/// use std::time::SystemTime;
///
/// use davidrs::telemetry::{Metrics, Unit};
///
/// let line = Metrics::new("Shop")
///     .dimension("Operation", "checkout")
///     .metric("Latency", 12.5, Unit::Milliseconds)
///     .property("orderId", "o-1")
///     .to_json(SystemTime::now())?;
/// assert!(line.contains(r#""Latency":12.5"#));
/// # Ok::<(), davidrs::RuntimeError>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Metrics {
    namespace: String,
    dimensions: BTreeMap<String, String>,
    metrics: Vec<(String, Unit)>,
    values: BTreeMap<String, f64>,
    properties: BTreeMap<String, serde_json::Value>,
}

impl Metrics {
    /// An empty document in `namespace`.
    #[must_use]
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            dimensions: BTreeMap::new(),
            metrics: Vec::new(),
            values: BTreeMap::new(),
            properties: BTreeMap::new(),
        }
    }

    /// Adds a dimension, or updates one already set. A new dimension past
    /// [`MAX_DIMENSIONS`] is ignored.
    #[must_use]
    pub fn dimension(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        if self.dimensions.len() < MAX_DIMENSIONS || self.dimensions.contains_key(&name) {
            self.dimensions.insert(name, value.into());
        }
        self
    }

    /// Records a metric, or updates the value of one already recorded (its
    /// unit stays the first one given), so a name is never declared twice. A
    /// new metric past [`MAX_METRICS`] is ignored.
    ///
    /// A value that is not finite is written as `null`, which is not a valid
    /// metric value.
    #[must_use]
    pub fn metric(mut self, name: impl Into<String>, value: f64, unit: Unit) -> Self {
        let name = name.into();
        match self.values.entry(name) {
            std::collections::btree_map::Entry::Occupied(mut existing) => {
                existing.insert(value);
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                if self.metrics.len() < MAX_METRICS {
                    self.metrics.push((slot.key().clone(), unit));
                    slot.insert(value);
                }
            }
        }
        self
    }

    /// Adds a property: a searchable field of the log line that is not a
    /// metric, such as a request id.
    #[must_use]
    pub fn property(
        mut self,
        name: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.properties.insert(name.into(), value.into());
        self
    }

    /// Renders the document as one line of JSON, stamped with `at`.
    ///
    /// EMF writes the time as whole milliseconds since the Unix epoch; a time
    /// before the epoch is written as `0`.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`](crate::RuntimeError) when the document cannot
    /// be serialized.
    pub fn to_json(&self, at: SystemTime) -> Result<String, crate::RuntimeError> {
        let timestamp = at.duration_since(UNIX_EPOCH).unwrap_or_default();
        let mut root = serde_json::Map::new();
        root.insert(
            "_aws".to_owned(),
            serde_json::json!({
                "Timestamp": timestamp.as_millis() as u64,
                "CloudWatchMetrics": [{
                    "Namespace": self.namespace,
                    "Dimensions": [self.dimensions.keys().collect::<Vec<_>>()],
                    "Metrics": self.metrics.iter().map(|(name, unit)| serde_json::json!({ "Name": name, "Unit": unit })).collect::<Vec<_>>(),
                }],
            }),
        );
        for (name, value) in &self.dimensions {
            root.insert(name.clone(), serde_json::Value::String(value.clone()));
        }
        for (name, value) in &self.values {
            root.insert(name.clone(), serde_json::json!(value));
        }
        for (name, value) in &self.properties {
            root.insert(name.clone(), value.clone());
        }
        serde_json::to_string(&root)
            .map_err(|error| crate::RuntimeError::other("serializing metrics", error))
    }

    /// How many metrics this document carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.metrics.len()
    }

    /// Whether the document has no metrics.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.metrics.is_empty()
    }
}
