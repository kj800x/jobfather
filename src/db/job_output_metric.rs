use std::collections::BTreeMap;

use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

use crate::job_metrics::{JobMetric, MetricKind};

/// Current value of one job output metric series.
pub struct StoredMetric {
    pub namespace: String,
    pub job_template: String,
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub kind: MetricKind,
    pub value: f64,
    pub updated_unix: i64,
}

/// Record metrics reported by one job run. Gauges replace the stored value;
/// counters add to it unless `include_counters` is false (e.g. on a re-upload
/// of the same run's metrics.json).
pub fn record(
    namespace: &str,
    job_template: &str,
    job_name: &str,
    metrics: &[JobMetric],
    include_counters: bool,
    conn: &mut PooledConnection<SqliteConnectionManager>,
) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    for m in metrics {
        if m.kind == MetricKind::Counter && !include_counters {
            continue;
        }
        let labels_json = serde_json::to_string(&m.labels).unwrap_or_else(|_| "{}".to_string());
        let sql = match m.kind {
            MetricKind::Gauge => {
                "INSERT INTO job_output_metric
                    (namespace, job_template, name, labels_json, kind, value, job_name, updated_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CAST(strftime('%s', 'now') AS INTEGER))
                 ON CONFLICT(namespace, job_template, name, labels_json, kind) DO UPDATE SET
                    value = excluded.value,
                    job_name = excluded.job_name,
                    updated_unix = excluded.updated_unix"
            }
            MetricKind::Counter => {
                "INSERT INTO job_output_metric
                    (namespace, job_template, name, labels_json, kind, value, job_name, updated_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CAST(strftime('%s', 'now') AS INTEGER))
                 ON CONFLICT(namespace, job_template, name, labels_json, kind) DO UPDATE SET
                    value = job_output_metric.value + excluded.value,
                    job_name = excluded.job_name,
                    updated_unix = excluded.updated_unix"
            }
        };
        tx.execute(
            sql,
            params![
                namespace,
                job_template,
                m.name,
                labels_json,
                m.kind.as_str(),
                m.value,
                job_name
            ],
        )?;
    }
    tx.commit()
}

pub fn all(
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Vec<StoredMetric>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT namespace, job_template, name, labels_json, kind, value, updated_unix
         FROM job_output_metric",
    )?;
    let rows = stmt.query_map([], |row| {
        let labels_json: String = row.get(3)?;
        let kind: String = row.get(4)?;
        Ok(StoredMetric {
            namespace: row.get(0)?,
            job_template: row.get(1)?,
            name: row.get(2)?,
            labels: serde_json::from_str(&labels_json).unwrap_or_default(),
            kind: if kind == "counter" {
                MetricKind::Counter
            } else {
                MetricKind::Gauge
            },
            value: row.get(5)?,
            updated_unix: row.get(6)?,
        })
    })?;
    rows.collect()
}
