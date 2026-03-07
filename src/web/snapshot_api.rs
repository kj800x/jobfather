use actix_web::{get, post, web, HttpResponse, Responder};
use maud::html;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::Deserialize;

use crate::db::ArchivedJob;

#[derive(Deserialize)]
pub struct AcceptSnapshotsForm {
    pub job_template_name: Option<String>,
    pub job_template_namespace: Option<String>,
}

/// Accept the current job's snapshots as the new baseline for its JobTemplate.
#[post("/api/jobs/{namespace}/{name}/snapshots/accept")]
pub async fn accept_snapshots(
    path: web::Path<(String, String)>,
    form: web::Form<AcceptSnapshotsForm>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };

    let archived = ArchivedJob::get_by_name_and_namespace(&name, &namespace, &conn)
        .ok()
        .flatten();

    // Get the tarball — try job_output table (live), then archived_job
    let tarball = crate::db::job_output::get(&name, &namespace, "test-snapshots.tar.gz", &conn)
        .ok()
        .flatten()
        .or_else(|| archived.as_ref().and_then(|j| j.output_test_snapshots.clone()));

    let tarball = match tarball {
        Some(t) => t,
        None => return HttpResponse::NotFound().body("No test snapshots found for this job"),
    };

    // Determine the JobTemplate name/namespace from form params or archived job
    let (jt_name, jt_namespace) = match (&form.job_template_name, &form.job_template_namespace) {
        (Some(n), Some(ns)) => (n.clone(), ns.clone()),
        _ => match &archived {
            Some(j) => (j.job_template_name.clone(), j.job_template_namespace.clone()),
            None => {
                return HttpResponse::BadRequest()
                    .body("Cannot determine JobTemplate. Provide job_template_name and job_template_namespace.");
            }
        },
    };

    // Only allow accepting from the latest run
    if !crate::db::snapshot::is_latest_snapshot_job(&jt_name, &jt_namespace, &name, &namespace, &conn) {
        return HttpResponse::BadRequest().body("Only the latest run can be accepted as baseline");
    }

    // Extract the tarball
    let files = match crate::snapshot::extract_tarball(&tarball) {
        Ok(f) => f,
        Err(e) => return HttpResponse::InternalServerError().body(format!("Failed to extract snapshots: {}", e)),
    };

    // Create a new baseline set (old baselines are preserved but marked not latest)
    if let Err(e) = crate::db::snapshot::create_baseline(&jt_name, &jt_namespace, &files, &conn) {
        log::error!("Failed to create baseline: {}", e);
        return HttpResponse::InternalServerError().body("Failed to create baseline");
    }

    // Update snapshot status in both job_output (live) and archived_job tables
    let _ = crate::db::job_output::upsert(
        &name,
        &namespace,
        "_snapshot_status",
        b"accepted_as_baseline",
        &conn,
    );
    let _ = crate::db::snapshot::update_archived_snapshot_status(
        &name,
        &namespace,
        "accepted_as_baseline",
        &conn,
    );

    log::info!(
        "Accepted snapshots from {}/{} as new baseline for {}/{}",
        namespace, name, jt_namespace, jt_name
    );

    let n_files = files.len();

    // Return a re-rendered snapshot suite showing accepted state
    let markup = html! {
        div id="snapshot-suite" class="test-suite" {
            div class="test-suite-header" {
                span class="test-suite-name" { "snapshots" }
                span class="test-suite-stats" { (n_files) " files" }
            }
            @for path in files.keys() {
                div class="test-case test-case-info" {
                    div class="test-case-header" {
                        span class="test-case-icon" { "\u{0394}" }
                        span class="test-case-name" { (path) }
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

/// View a file from the job's current test snapshots tarball.
#[get("/jobs/{namespace}/{name}/snapshots/current/{path:.*}")]
pub async fn job_snapshot_file(
    path: web::Path<(String, String, String)>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name, file_path) = path.into_inner();

    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return HttpResponse::InternalServerError().body("Database error"),
    };

    // Try job_output table (live), then archived_job
    let tarball = crate::db::job_output::get(&name, &namespace, "test-snapshots.tar.gz", &conn)
        .ok()
        .flatten()
        .or_else(|| {
            ArchivedJob::get_by_name_and_namespace(&name, &namespace, &conn)
                .ok()
                .flatten()
                .and_then(|j| j.output_test_snapshots)
        });

    let tarball = match tarball {
        Some(t) => t,
        None => return HttpResponse::NotFound().body("No test snapshots found"),
    };

    match crate::snapshot::extract_file_from_tarball(&tarball, &file_path) {
        Ok(Some(content)) => {
            let content_type = guess_content_type(&file_path);
            HttpResponse::Ok()
                .content_type(content_type)
                .body(content)
        }
        Ok(None) => HttpResponse::NotFound().body(format!("File not found: {}", file_path)),
        Err(e) => HttpResponse::InternalServerError().body(format!("Failed to extract: {}", e)),
    }
}

/// View a baseline file for a JobTemplate (latest baseline).
#[get("/job-templates/{namespace}/{name}/snapshots/baseline/{path:.*}")]
pub async fn baseline_snapshot_file(
    path: web::Path<(String, String, String)>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name, file_path) = path.into_inner();

    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return HttpResponse::InternalServerError().body("Database error"),
    };

    match crate::db::snapshot::get_latest_baseline_file(&name, &namespace, &file_path, &conn) {
        Ok(Some(content)) => {
            let content_type = guess_content_type(&file_path);
            HttpResponse::Ok()
                .content_type(content_type)
                .body(content)
        }
        Ok(None) => HttpResponse::NotFound().body(format!("Baseline file not found: {}", file_path)),
        Err(e) => {
            log::error!("Failed to load baseline file: {}", e);
            HttpResponse::InternalServerError().body("Database error")
        }
    }
}

/// View a file from a specific baseline set by ID.
#[get("/snapshots/baseline/{baseline_id}/{path:.*}")]
pub async fn baseline_set_file(
    path: web::Path<(i64, String)>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (baseline_id, file_path) = path.into_inner();

    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return HttpResponse::InternalServerError().body("Database error"),
    };

    match crate::db::snapshot::get_baseline_file(baseline_id, &file_path, &conn) {
        Ok(Some(content)) => {
            let content_type = guess_content_type(&file_path);
            HttpResponse::Ok()
                .content_type(content_type)
                .body(content)
        }
        Ok(None) => HttpResponse::NotFound().body(format!("Baseline file not found: {}", file_path)),
        Err(e) => {
            log::error!("Failed to load baseline file: {}", e);
            HttpResponse::InternalServerError().body("Database error")
        }
    }
}

fn guess_content_type(path: &str) -> &'static str {
    if path.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".xml") {
        "application/xml; charset=utf-8"
    } else if path.ends_with(".html") || path.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    }
}
