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
        M::up(
            "ALTER TABLE archived_job ADD COLUMN output_test_snapshots BLOB;",
        ),
        M::up(
            "CREATE TABLE IF NOT EXISTS snapshot_baseline (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_template_name TEXT NOT NULL,
                job_template_namespace TEXT NOT NULL,
                file_path TEXT NOT NULL,
                content BLOB NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(job_template_name, job_template_namespace, file_path)
            );
            ALTER TABLE archived_job ADD COLUMN snapshot_status TEXT;
            ALTER TABLE archived_job ADD COLUMN snapshot_diff_json TEXT;",
        ),
        M::up(
            "ALTER TABLE archived_job ADD COLUMN artifact_sha TEXT;
             ALTER TABLE archived_job ADD COLUMN config_sha TEXT;",
        ),
        M::up(
            "CREATE TABLE snapshot_baseline_set (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_template_name TEXT NOT NULL,
                job_template_namespace TEXT NOT NULL,
                is_latest INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE snapshot_baseline_file (
                baseline_set_id INTEGER NOT NULL REFERENCES snapshot_baseline_set(id),
                file_path TEXT NOT NULL,
                content BLOB NOT NULL,
                PRIMARY KEY (baseline_set_id, file_path)
            );
            INSERT INTO snapshot_baseline_set (job_template_name, job_template_namespace, is_latest)
                SELECT DISTINCT job_template_name, job_template_namespace, 1
                FROM snapshot_baseline;
            INSERT INTO snapshot_baseline_file (baseline_set_id, file_path, content)
                SELECT s.id, b.file_path, b.content
                FROM snapshot_baseline b
                JOIN snapshot_baseline_set s
                    ON s.job_template_name = b.job_template_name
                    AND s.job_template_namespace = b.job_template_namespace;
            DROP TABLE snapshot_baseline;
            ALTER TABLE archived_job ADD COLUMN snapshot_baseline_id INTEGER;",
        ),
    ]);

    migrations.to_latest(&mut conn)?;
    Ok(())
}
