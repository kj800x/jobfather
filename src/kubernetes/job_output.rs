use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::job_output as db;

pub struct JobOutput {
    pub result_json: Option<String>,
    pub report_md: Option<String>,
    pub test_results_xml: Option<String>,
    pub archive: Option<Vec<u8>>,
    pub test_snapshots: Option<Vec<u8>>,
}

impl JobOutput {
    pub fn empty() -> Self {
        Self {
            result_json: None,
            report_md: None,
            test_results_xml: None,
            archive: None,
            test_snapshots: None,
        }
    }

    /// Load job output from the job_output table (uploaded by sidecar).
    pub fn load(
        job_name: &str,
        namespace: &str,
        pool: &Pool<SqliteConnectionManager>,
    ) -> Self {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(_) => return Self::empty(),
        };

        let files = match db::get_all(job_name, namespace, &conn) {
            Ok(f) => f,
            Err(_) => return Self::empty(),
        };

        let mut output = Self::empty();
        for (name, content) in files {
            match name.as_str() {
                "result.json" => output.result_json = String::from_utf8(content).ok(),
                "report.md" => output.report_md = String::from_utf8(content).ok(),
                "test-results.xml" => output.test_results_xml = String::from_utf8(content).ok(),
                "archive.tar.gz" => output.archive = Some(content),
                "test-snapshots.tar.gz" => output.test_snapshots = Some(content),
                _ => {}
            }
        }
        output
    }
}
