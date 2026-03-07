use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use serde::{Deserialize, Serialize};

const SELECT_COLUMNS: &str =
    "id, name, namespace, job_template_name, job_template_namespace,
     uid, status, start_time, completion_time, duration_seconds, logs, archived_at,
     output_result_json, output_report_md, output_test_results_xml, output_archive,
     output_test_snapshots, events_json, snapshot_status, snapshot_diff_json,
     artifact_sha, config_sha, snapshot_baseline_id";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchivedJob {
    pub id: i64,
    pub name: String,
    pub namespace: String,
    pub job_template_name: String,
    pub job_template_namespace: String,
    pub uid: String,
    pub status: String,
    pub start_time: Option<String>,
    pub completion_time: Option<String>,
    pub duration_seconds: Option<i64>,
    pub logs: Option<String>,
    pub archived_at: String,
    pub output_result_json: Option<String>,
    pub output_report_md: Option<String>,
    pub output_test_results_xml: Option<String>,
    #[serde(skip)]
    pub output_archive: Option<Vec<u8>>,
    #[serde(skip)]
    pub output_test_snapshots: Option<Vec<u8>>,
    pub events_json: Option<String>,
    pub snapshot_status: Option<String>,
    pub snapshot_diff_json: Option<String>,
    pub artifact_sha: Option<String>,
    pub config_sha: Option<String>,
    pub snapshot_baseline_id: Option<i64>,
}

pub struct ArchivedJobEgg {
    pub name: String,
    pub namespace: String,
    pub job_template_name: String,
    pub job_template_namespace: String,
    pub uid: String,
    pub status: String,
    pub start_time: Option<String>,
    pub completion_time: Option<String>,
    pub duration_seconds: Option<i64>,
    pub logs: Option<String>,
    pub output_result_json: Option<String>,
    pub output_report_md: Option<String>,
    pub output_test_results_xml: Option<String>,
    pub output_archive: Option<Vec<u8>>,
    pub output_test_snapshots: Option<Vec<u8>>,
    pub events_json: Option<String>,
    pub snapshot_status: Option<String>,
    pub snapshot_diff_json: Option<String>,
    pub artifact_sha: Option<String>,
    pub config_sha: Option<String>,
    pub snapshot_baseline_id: Option<i64>,
}

impl ArchivedJob {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            namespace: row.get(2)?,
            job_template_name: row.get(3)?,
            job_template_namespace: row.get(4)?,
            uid: row.get(5)?,
            status: row.get(6)?,
            start_time: row.get(7)?,
            completion_time: row.get(8)?,
            duration_seconds: row.get(9)?,
            logs: row.get(10)?,
            archived_at: row.get(11)?,
            output_result_json: row.get(12)?,
            output_report_md: row.get(13)?,
            output_test_results_xml: row.get(14)?,
            output_archive: row.get(15)?,
            output_test_snapshots: row.get(16)?,
            events_json: row.get(17)?,
            snapshot_status: row.get(18)?,
            snapshot_diff_json: row.get(19)?,
            artifact_sha: row.get(20)?,
            config_sha: row.get(21)?,
            snapshot_baseline_id: row.get(22)?,
        })
    }

    pub fn get_by_job_template(
        job_template_name: &str,
        job_template_namespace: &str,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> Result<Vec<Self>, rusqlite::Error> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM archived_job
             WHERE job_template_name = ?1 AND job_template_namespace = ?2
             ORDER BY archived_at DESC",
            SELECT_COLUMNS
        ))?;
        let rows = stmt.query_and_then(
            params![job_template_name, job_template_namespace],
            Self::from_row,
        )?;
        rows.collect()
    }

    pub fn get_by_name_and_namespace(
        name: &str,
        namespace: &str,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> Result<Option<Self>, rusqlite::Error> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM archived_job WHERE name = ?1 AND namespace = ?2",
            SELECT_COLUMNS
        ))?;
        let mut rows = stmt.query_and_then(params![name, namespace], Self::from_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn upsert(
        egg: &ArchivedJobEgg,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> Result<(), rusqlite::Error> {
        conn.execute(
            "INSERT INTO archived_job
                (name, namespace, job_template_name, job_template_namespace,
                 uid, status, start_time, completion_time, duration_seconds, logs,
                 output_result_json, output_report_md, output_test_results_xml, output_archive,
                 output_test_snapshots, events_json, snapshot_status, snapshot_diff_json,
                 artifact_sha, config_sha, snapshot_baseline_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
             ON CONFLICT(uid) DO UPDATE SET
                status = excluded.status,
                completion_time = excluded.completion_time,
                duration_seconds = excluded.duration_seconds,
                logs = excluded.logs,
                output_result_json = excluded.output_result_json,
                output_report_md = excluded.output_report_md,
                output_test_results_xml = excluded.output_test_results_xml,
                output_archive = excluded.output_archive,
                output_test_snapshots = excluded.output_test_snapshots,
                events_json = excluded.events_json,
                snapshot_status = CASE
                    WHEN archived_job.snapshot_status = 'accepted_as_baseline'
                    THEN archived_job.snapshot_status
                    ELSE excluded.snapshot_status
                    END,
                snapshot_diff_json = CASE
                    WHEN archived_job.snapshot_status = 'accepted_as_baseline'
                    THEN archived_job.snapshot_diff_json
                    ELSE excluded.snapshot_diff_json
                    END,
                artifact_sha = excluded.artifact_sha,
                config_sha = excluded.config_sha,
                snapshot_baseline_id = CASE
                    WHEN archived_job.snapshot_baseline_id IS NOT NULL
                    THEN archived_job.snapshot_baseline_id
                    ELSE excluded.snapshot_baseline_id
                    END",
            params![
                egg.name,
                egg.namespace,
                egg.job_template_name,
                egg.job_template_namespace,
                egg.uid,
                egg.status,
                egg.start_time,
                egg.completion_time,
                egg.duration_seconds,
                egg.logs,
                egg.output_result_json,
                egg.output_report_md,
                egg.output_test_results_xml,
                egg.output_archive,
                egg.output_test_snapshots,
                egg.events_json,
                egg.snapshot_status,
                egg.snapshot_diff_json,
                egg.artifact_sha,
                egg.config_sha,
                egg.snapshot_baseline_id,
            ],
        )?;
        Ok(())
    }
}
