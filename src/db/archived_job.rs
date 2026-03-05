use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use serde::{Deserialize, Serialize};

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
        })
    }

    pub fn get_by_job_template(
        job_template_name: &str,
        job_template_namespace: &str,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> Result<Vec<Self>, rusqlite::Error> {
        let mut stmt = conn.prepare(
            "SELECT id, name, namespace, job_template_name, job_template_namespace,
                    uid, status, start_time, completion_time, duration_seconds, logs, archived_at
             FROM archived_job
             WHERE job_template_name = ?1 AND job_template_namespace = ?2
             ORDER BY archived_at DESC",
        )?;
        let rows = stmt.query_and_then(
            params![job_template_name, job_template_namespace],
            Self::from_row,
        )?;
        rows.collect()
    }

    pub fn upsert(
        egg: &ArchivedJobEgg,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> Result<(), rusqlite::Error> {
        conn.execute(
            "INSERT INTO archived_job
                (name, namespace, job_template_name, job_template_namespace,
                 uid, status, start_time, completion_time, duration_seconds, logs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(uid) DO UPDATE SET
                status = excluded.status,
                completion_time = excluded.completion_time,
                duration_seconds = excluded.duration_seconds,
                logs = excluded.logs",
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
            ],
        )?;
        Ok(())
    }
}
