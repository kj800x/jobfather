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
    ]);

    migrations.to_latest(&mut conn)?;
    Ok(())
}
