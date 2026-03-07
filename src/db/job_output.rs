use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

const VALID_FILES: &[&str] = &[
    "result.json",
    "report.md",
    "test-results.xml",
    "archive.tar.gz",
    "test-snapshots.tar.gz",
];

pub fn is_valid_file(name: &str) -> bool {
    VALID_FILES.contains(&name)
}

pub fn upsert(
    job_name: &str,
    namespace: &str,
    file_name: &str,
    content: &[u8],
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO job_output (job_name, namespace, file_name, content)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(job_name, namespace, file_name) DO UPDATE SET
            content = excluded.content,
            uploaded_at = datetime('now')",
        params![job_name, namespace, file_name, content],
    )?;
    Ok(())
}

pub fn get(
    job_name: &str,
    namespace: &str,
    file_name: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Option<Vec<u8>>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT content FROM job_output
         WHERE job_name = ?1 AND namespace = ?2 AND file_name = ?3",
    )?;
    let mut rows = stmt.query(params![job_name, namespace, file_name])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

pub fn get_all(
    job_name: &str,
    namespace: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Vec<(String, Vec<u8>)>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT file_name, content FROM job_output
         WHERE job_name = ?1 AND namespace = ?2",
    )?;
    let rows = stmt.query_map(params![job_name, namespace], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    rows.collect()
}

pub fn get_string(
    job_name: &str,
    namespace: &str,
    file_name: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Option<String> {
    get(job_name, namespace, file_name, conn)
        .ok()
        .flatten()
        .and_then(|b| String::from_utf8(b).ok())
}

pub fn delete_for_job(
    job_name: &str,
    namespace: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM job_output WHERE job_name = ?1 AND namespace = ?2",
        params![job_name, namespace],
    )?;
    Ok(())
}
