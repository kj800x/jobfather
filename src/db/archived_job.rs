use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use serde::{Deserialize, Serialize};

const SELECT_COLUMNS: &str =
    "id, name, namespace, job_template_name, job_template_namespace,
     uid, status, start_time, completion_time, duration_seconds, logs, archived_at,
     output_result_json, output_report_md, output_test_results_xml, output_archive,
     events_json";

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
    pub events_json: Option<String>,
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
    pub events_json: Option<String>,
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
            events_json: row.get(16)?,
        })
    }

    pub fn has_output(&self) -> bool {
        self.output_result_json.is_some()
            || self.output_report_md.is_some()
            || self.output_test_results_xml.is_some()
            || self.output_archive.is_some()
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
                 events_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(uid) DO UPDATE SET
                status = excluded.status,
                completion_time = excluded.completion_time,
                duration_seconds = excluded.duration_seconds,
                logs = excluded.logs,
                output_result_json = excluded.output_result_json,
                output_report_md = excluded.output_report_md,
                output_test_results_xml = excluded.output_test_results_xml,
                output_archive = excluded.output_archive,
                events_json = excluded.events_json",
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
                egg.events_json,
            ],
        )?;
        Ok(())
    }
}
