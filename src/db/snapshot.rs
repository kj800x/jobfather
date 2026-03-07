use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use std::collections::BTreeMap;

/// Get the latest baseline set ID for a JobTemplate. Returns None if no baseline exists.
pub fn get_latest_baseline_id(
    job_template_name: &str,
    job_template_namespace: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Option<i64> {
    conn.query_row(
        "SELECT id FROM snapshot_baseline_set
         WHERE job_template_name = ?1 AND job_template_namespace = ?2 AND is_latest = 1",
        params![job_template_name, job_template_namespace],
        |row| row.get(0),
    )
    .ok()
}

/// Load all files for a specific baseline set.
pub fn load_baseline_files(
    baseline_id: i64,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> BTreeMap<String, Vec<u8>> {
    let mut stmt = match conn.prepare(
        "SELECT file_path, content FROM snapshot_baseline_file
         WHERE baseline_set_id = ?1",
    ) {
        Ok(s) => s,
        Err(_) => return BTreeMap::new(),
    };

    let rows = match stmt.query_map(params![baseline_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    }) {
        Ok(r) => r,
        Err(_) => return BTreeMap::new(),
    };

    rows.filter_map(|r| r.ok()).collect()
}

/// Load the latest baseline for a JobTemplate.
pub fn load_latest_baseline_conn(
    job_template_name: &str,
    job_template_namespace: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> (Option<i64>, BTreeMap<String, Vec<u8>>) {
    match get_latest_baseline_id(job_template_name, job_template_namespace, conn) {
        Some(id) => (Some(id), load_baseline_files(id, conn)),
        None => (None, BTreeMap::new()),
    }
}

/// Load a single file from a specific baseline set.
pub fn get_baseline_file(
    baseline_id: i64,
    file_path: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Option<Vec<u8>>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT content FROM snapshot_baseline_file
         WHERE baseline_set_id = ?1 AND file_path = ?2",
    )?;
    let mut rows = stmt.query(params![baseline_id, file_path])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Load a single file from the latest baseline for a JobTemplate.
pub fn get_latest_baseline_file(
    job_template_name: &str,
    job_template_namespace: &str,
    file_path: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Option<Vec<u8>>, rusqlite::Error> {
    match get_latest_baseline_id(job_template_name, job_template_namespace, conn) {
        Some(id) => get_baseline_file(id, file_path, conn),
        None => Ok(None),
    }
}

/// Create a new baseline set for a JobTemplate, marking any previous baseline as not latest.
/// Returns the new baseline set ID.
pub fn create_baseline(
    job_template_name: &str,
    job_template_namespace: &str,
    files: &BTreeMap<String, Vec<u8>>,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<i64, rusqlite::Error> {
    // Mark existing baselines as not latest
    conn.execute(
        "UPDATE snapshot_baseline_set SET is_latest = 0
         WHERE job_template_name = ?1 AND job_template_namespace = ?2 AND is_latest = 1",
        params![job_template_name, job_template_namespace],
    )?;

    // Insert new baseline set
    conn.execute(
        "INSERT INTO snapshot_baseline_set (job_template_name, job_template_namespace, is_latest)
         VALUES (?1, ?2, 1)",
        params![job_template_name, job_template_namespace],
    )?;
    let set_id = conn.last_insert_rowid();

    // Insert files
    let mut stmt = conn.prepare(
        "INSERT INTO snapshot_baseline_file (baseline_set_id, file_path, content)
         VALUES (?1, ?2, ?3)",
    )?;
    for (path, content) in files {
        stmt.execute(params![set_id, path, content])?;
    }

    Ok(set_id)
}

/// Update the snapshot_status of an archived job.
pub fn update_archived_snapshot_status(
    job_name: &str,
    namespace: &str,
    status: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE archived_job SET snapshot_status = ?1 WHERE name = ?2 AND namespace = ?3",
        params![status, job_name, namespace],
    )?;
    Ok(())
}

/// Check if a given job is the latest job with snapshots for its template.
/// Checks both archived_job (archived) and job_output (live) tables.
/// Uses lexicographic ordering on job names (which contain timestamps).
pub fn is_latest_snapshot_job(
    job_template_name: &str,
    job_template_namespace: &str,
    job_name: &str,
    namespace: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> bool {
    // Check if any archived job with snapshots has a later name
    let has_newer_archived: bool = conn
        .query_row(
            "SELECT 1 FROM archived_job
             WHERE job_template_name = ?1 AND job_template_namespace = ?2
               AND output_test_snapshots IS NOT NULL
               AND name > ?3
             LIMIT 1",
            params![job_template_name, job_template_namespace, job_name],
            |_| Ok(true),
        )
        .unwrap_or(false);

    if has_newer_archived {
        return false;
    }

    // Check if any live job (in job_output) with snapshots has a later name.
    // Live job names follow the pattern "{template_name}-{timestamp}".
    let prefix = format!("{}-%", job_template_name);
    let has_newer_live: bool = conn
        .query_row(
            "SELECT 1 FROM job_output
             WHERE namespace = ?1 AND file_name = 'test-snapshots.tar.gz'
               AND job_name LIKE ?2
               AND job_name > ?3
             LIMIT 1",
            params![namespace, prefix, job_name],
            |_| Ok(true),
        )
        .unwrap_or(false);

    !has_newer_live
}
