use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite_migration::{Migrations, M};

pub fn migrate(
    mut conn: PooledConnection<SqliteConnectionManager>,
) -> Result<(), Box<dyn std::error::Error>> {
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;

    let migrations = Migrations::new(vec![
        M::up(
            "CREATE TABLE IF NOT EXISTS archived_job (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                namespace TEXT NOT NULL,
                job_template_name TEXT NOT NULL,
                job_template_namespace TEXT NOT NULL,
                uid TEXT NOT NULL UNIQUE,
                status TEXT NOT NULL,
                start_time TEXT,
                completion_time TEXT,
                duration_seconds INTEGER,
                logs TEXT,
                archived_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        ),
        M::up(
            "ALTER TABLE archived_job ADD COLUMN output_result_json TEXT;
             ALTER TABLE archived_job ADD COLUMN output_report_md TEXT;
             ALTER TABLE archived_job ADD COLUMN output_test_results_xml TEXT;
             ALTER TABLE archived_job ADD COLUMN output_archive BLOB;",
        ),
        M::up(
            "ALTER TABLE archived_job ADD COLUMN events_json TEXT;",
        ),
        M::up(
            "CREATE TABLE IF NOT EXISTS job_output (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_name TEXT NOT NULL,
                namespace TEXT NOT NULL,
                file_name TEXT NOT NULL,
                content BLOB NOT NULL,
                uploaded_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(job_name, namespace, file_name)
            );",
        ),
    ]);

    migrations.to_latest(&mut conn)?;
    Ok(())
}
