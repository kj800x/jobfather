//! Metrics emitted by jobs via `/job-output/metrics.json` and re-exported on
//! `/api/metrics`.
//!
//! Gauges hold the value from the most recent run that reported them. Counters
//! accumulate the reported value across runs. Both are persisted in SQLite
//! (`job_output_metric`) so they survive Jobfather restarts.

use std::collections::BTreeMap;

use prometheus::core::{Collector, Desc};
use prometheus::proto;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::job_output_metric::{self, StoredMetric};

pub const GAUGE_METRIC: &str = "jobfather_job_output_gauge";
pub const GAUGE_TIMESTAMP_METRIC: &str = "jobfather_job_output_gauge_timestamp_seconds";
pub const COUNTER_METRIC: &str = "jobfather_job_output_total";

/// Label names Jobfather sets itself; jobs may not use them.
const RESERVED_LABELS: &[&str] = &["namespace", "job_template", "name"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    Gauge,
    Counter,
}

impl MetricKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MetricKind::Gauge => "gauge",
            MetricKind::Counter => "counter",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JobMetric {
    pub name: String,
    pub value: f64,
    pub kind: MetricKind,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct ParsedMetrics {
    pub metrics: Vec<JobMetric>,
    pub warnings: Vec<String>,
}

/// Leniently parse metrics.json. Invalid entries are skipped and reported in
/// `warnings` rather than failing the whole file.
pub fn parse(json: &str) -> ParsedMetrics {
    let mut parsed = ParsedMetrics::default();

    let entries = match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Array(entries)) => entries,
        Ok(_) => {
            parsed
                .warnings
                .push("metrics.json must be a JSON array".to_string());
            return parsed;
        }
        Err(e) => {
            parsed.warnings.push(format!("Invalid JSON: {}", e));
            return parsed;
        }
    };

    'entries: for (i, entry) in entries.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            parsed
                .warnings
                .push(format!("Metric {}: not an object, skipped", i));
            continue;
        };

        let Some(name) = obj.get("name").and_then(|n| n.as_str()) else {
            parsed
                .warnings
                .push(format!("Metric {}: missing string \"name\", skipped", i));
            continue;
        };

        let Some(value) = obj
            .get("value")
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite())
        else {
            parsed.warnings.push(format!(
                "Metric {} ({}): \"value\" must be a finite number, skipped",
                i, name
            ));
            continue;
        };

        let kind = match obj.get("type").and_then(|t| t.as_str()) {
            None | Some("gauge") => MetricKind::Gauge,
            Some("counter") => MetricKind::Counter,
            Some(other) => {
                parsed.warnings.push(format!(
                    "Metric {} ({}): unknown type \"{}\" (expected gauge or counter), skipped",
                    i, name, other
                ));
                continue;
            }
        };

        if kind == MetricKind::Counter && value < 0.0 {
            parsed.warnings.push(format!(
                "Metric {} ({}): counter value must not be negative, skipped",
                i, name
            ));
            continue;
        }

        let mut labels = BTreeMap::new();
        match obj.get("labels") {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (key, val) in map {
                    if !is_valid_label_name(key) || RESERVED_LABELS.contains(&key.as_str()) {
                        parsed.warnings.push(format!(
                            "Metric {} ({}): invalid or reserved label name \"{}\", skipped",
                            i, name, key
                        ));
                        continue 'entries;
                    }
                    let val = match val {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                            val.to_string()
                        }
                        _ => {
                            parsed.warnings.push(format!(
                                "Metric {} ({}): label \"{}\" must be a string, number, or boolean, skipped",
                                i, name, key
                            ));
                            continue 'entries;
                        }
                    };
                    labels.insert(key.clone(), val);
                }
            }
            Some(_) => {
                parsed.warnings.push(format!(
                    "Metric {} ({}): \"labels\" must be an object, skipped",
                    i, name
                ));
                continue;
            }
        }

        parsed.metrics.push(JobMetric {
            name: name.to_string(),
            value,
            kind,
            labels,
        });
    }

    parsed
}

/// Prometheus label name rules: `[a-zA-Z_][a-zA-Z0-9_]*`, not starting with `__`.
fn is_valid_label_name(name: &str) -> bool {
    let mut chars = name.chars();
    let valid_first = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    valid_first && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with("__")
}

/// Exposes stored job output metrics at scrape time.
pub struct JobOutputMetricsCollector {
    pool: Pool<SqliteConnectionManager>,
    descs: Vec<Desc>,
}

impl JobOutputMetricsCollector {
    pub fn new(pool: Pool<SqliteConnectionManager>) -> Self {
        let desc = |name: &str, help: &str| {
            Desc::new(
                name.to_string(),
                help.to_string(),
                vec![],
                Default::default(),
            )
            .expect("valid metric desc")
        };
        Self {
            pool,
            descs: vec![
                desc(
                    GAUGE_METRIC,
                    "Gauge reported by a job via metrics.json; holds the value from the most recent run that reported it",
                ),
                desc(
                    GAUGE_TIMESTAMP_METRIC,
                    "Unix time when the gauge value in jobfather_job_output_gauge was last reported",
                ),
                desc(
                    COUNTER_METRIC,
                    "Counter reported by jobs via metrics.json; accumulates each run's value",
                ),
            ],
        }
    }
}

fn family(
    name: &str,
    help: &str,
    kind: proto::MetricType,
    metrics: Vec<proto::Metric>,
) -> proto::MetricFamily {
    let mut mf = proto::MetricFamily::default();
    mf.set_name(name.to_string());
    mf.set_help(help.to_string());
    mf.set_field_type(kind);
    mf.set_metric(metrics.into());
    mf
}

fn label_pairs(stored: &StoredMetric) -> Vec<proto::LabelPair> {
    let mut labels: BTreeMap<&str, &str> = stored
        .labels
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    labels.insert("namespace", &stored.namespace);
    labels.insert("job_template", &stored.job_template);
    labels.insert("name", &stored.name);
    labels
        .into_iter()
        .map(|(k, v)| {
            let mut lp = proto::LabelPair::default();
            lp.set_name(k.to_string());
            lp.set_value(v.to_string());
            lp
        })
        .collect()
}

impl Collector for JobOutputMetricsCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.descs.iter().collect()
    }

    fn collect(&self) -> Vec<proto::MetricFamily> {
        let stored = match self.pool.get() {
            Ok(conn) => job_output_metric::all(&conn).unwrap_or_else(|e| {
                log::warn!("Failed to load job output metrics: {}", e);
                vec![]
            }),
            Err(_) => return vec![],
        };

        let mut gauges = Vec::new();
        let mut gauge_timestamps = Vec::new();
        let mut counters = Vec::new();

        for m in &stored {
            let labels = label_pairs(m);
            match m.kind {
                MetricKind::Gauge => {
                    let mut g = proto::Gauge::default();
                    g.set_value(m.value);
                    let mut metric = proto::Metric::default();
                    metric.set_label(labels.clone().into());
                    metric.set_gauge(g);
                    gauges.push(metric);

                    let mut ts = proto::Gauge::default();
                    ts.set_value(m.updated_unix as f64);
                    let mut metric = proto::Metric::default();
                    metric.set_label(labels.into());
                    metric.set_gauge(ts);
                    gauge_timestamps.push(metric);
                }
                MetricKind::Counter => {
                    let mut c = proto::Counter::default();
                    c.set_value(m.value);
                    let mut metric = proto::Metric::default();
                    metric.set_label(labels.into());
                    metric.set_counter(c);
                    counters.push(metric);
                }
            }
        }

        let help = |name: &str| {
            self.descs
                .iter()
                .find(|d| d.fq_name == name)
                .map(|d| d.help.clone())
                .unwrap_or_default()
        };

        vec![
            family(
                GAUGE_METRIC,
                &help(GAUGE_METRIC),
                proto::MetricType::GAUGE,
                gauges,
            ),
            family(
                GAUGE_TIMESTAMP_METRIC,
                &help(GAUGE_TIMESTAMP_METRIC),
                proto::MetricType::GAUGE,
                gauge_timestamps,
            ),
            family(
                COUNTER_METRIC,
                &help(COUNTER_METRIC),
                proto::MetricType::COUNTER,
                counters,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gauges_and_counters() {
        let parsed = parse(
            r#"[
                {"name": "queue_depth", "value": 17},
                {"name": "latency_p95_seconds", "value": 0.42, "labels": {"endpoint": "/api", "shard": 3}},
                {"name": "rows_processed", "value": 12345, "type": "counter"}
            ]"#,
        );
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(parsed.metrics.len(), 3);
        assert_eq!(parsed.metrics[0].kind, MetricKind::Gauge);
        assert_eq!(parsed.metrics[1].labels["endpoint"], "/api");
        assert_eq!(parsed.metrics[1].labels["shard"], "3");
        assert_eq!(parsed.metrics[2].kind, MetricKind::Counter);
        assert_eq!(parsed.metrics[2].value, 12345.0);
    }

    #[test]
    fn skips_invalid_entries_with_warnings() {
        let parsed = parse(
            r#"[
                {"name": "ok", "value": 1},
                {"name": "str", "value": "1"},
                {"name": "neg", "value": -1, "type": "counter"},
                {"name": "hist", "value": 1, "type": "histogram"},
                {"name": "reserved", "value": 1, "labels": {"namespace": "x"}},
                {"name": "badlabel", "value": 1, "labels": {"1abc": "x"}},
                {"name": "dunder", "value": 1, "labels": {"__x": "x"}},
                {"value": 1}
            ]"#,
        );
        assert_eq!(parsed.metrics.len(), 1);
        assert_eq!(parsed.warnings.len(), 7);
    }

    #[test]
    fn records_and_exports_metrics() {
        let pool = r2d2::Pool::builder()
            .max_size(1)
            .build(SqliteConnectionManager::memory())
            .unwrap();
        crate::db::migrations::migrate(pool.get().unwrap()).unwrap();

        let run = |json: &str, include_counters: bool| {
            let parsed = parse(json);
            job_output_metric::record(
                "ns",
                "tmpl",
                "tmpl-1",
                &parsed.metrics,
                include_counters,
                &mut pool.get().unwrap(),
            )
            .unwrap();
        };
        run(
            r#"[{"name": "depth", "value": 5, "labels": {"q": "a"}}, {"name": "rows", "value": 10, "type": "counter"}]"#,
            true,
        );
        run(
            r#"[{"name": "depth", "value": 7, "labels": {"q": "a"}}, {"name": "rows", "value": 4, "type": "counter"}]"#,
            true,
        );
        // A re-upload of the same run must not double-count counters
        run(
            r#"[{"name": "rows", "value": 4, "type": "counter"}]"#,
            false,
        );

        let registry = prometheus::Registry::new();
        registry
            .register(Box::new(JobOutputMetricsCollector::new(pool.clone())))
            .unwrap();
        let text = prometheus::TextEncoder::new()
            .encode_to_string(&registry.gather())
            .unwrap();

        assert!(
            text.contains(r#"jobfather_job_output_gauge{job_template="tmpl",name="depth",namespace="ns",q="a"} 7"#),
            "{}",
            text
        );
        assert!(
            text.contains(
                r#"jobfather_job_output_total{job_template="tmpl",name="rows",namespace="ns"} 14"#
            ),
            "{}",
            text
        );
        assert!(text.contains("# TYPE jobfather_job_output_total counter"));
        assert!(text.contains("jobfather_job_output_gauge_timestamp_seconds{"));
    }

    #[test]
    fn negative_gauges_are_allowed() {
        let parsed = parse(r#"[{"name": "delta", "value": -3.5}]"#);
        assert!(parsed.warnings.is_empty());
        assert_eq!(parsed.metrics[0].value, -3.5);
    }
}
