use actix_web::{put, web, HttpResponse, Responder};
use k8s_openapi::api::batch::v1::Job;
use kube::{Api, Client};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::job_output;

#[put("/api/jobs/{namespace}/{name}/output/{filename}")]
pub async fn upload_job_output(
    path: web::Path<(String, String, String)>,
    body: web::Bytes,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    client: web::Data<Client>,
) -> impl Responder {
    let (namespace, name, filename) = path.into_inner();

    if !job_output::is_valid_file(&filename) {
        return HttpResponse::BadRequest().body(format!("Invalid output file: {}", filename));
    }

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };

    match job_output::upsert(&name, &namespace, &filename, &body, &conn) {
        Ok(_) => {
            log::info!("Received output {}/{}/{}", namespace, name, filename);
        }
        Err(e) => {
            log::error!("Failed to store output {}/{}/{}: {}", namespace, name, filename, e);
            return HttpResponse::InternalServerError().body("Failed to store output");
        }
    }

    // When test-snapshots.tar.gz is uploaded, compute and store snapshot status
    if filename == "test-snapshots.tar.gz" {
        compute_and_store_snapshot_status(&name, &namespace, &body, &pool, &client).await;
    }

    HttpResponse::Ok().body("OK")
}

async fn compute_and_store_snapshot_status(
    job_name: &str,
    namespace: &str,
    tarball: &[u8],
    pool: &Pool<SqliteConnectionManager>,
    client: &Client,
) {
    // Look up the Job in K8s to find its owning JobTemplate
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
    let jt_name = match job_api.get(job_name).await {
        Ok(job) => job
            .metadata
            .owner_references
            .as_ref()
            .and_then(|refs| refs.iter().find(|r| r.kind == "JobTemplate"))
            .map(|r| r.name.clone()),
        Err(e) => {
            log::warn!("Failed to look up job {}/{} for snapshot status: {}", namespace, job_name, e);
            None
        }
    };

    let Some(jt_name) = jt_name else {
        return;
    };

    let current = match crate::snapshot::extract_tarball(tarball) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("Failed to extract snapshots for {}/{}: {}", namespace, job_name, e);
            return;
        }
    };

    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return,
    };

    let (baseline_id, baseline) = crate::db::snapshot::load_latest_baseline_conn(&jt_name, namespace, &conn);
    let diff = crate::snapshot::compare(&baseline, &current);
    let status = if diff.has_differences() {
        "differs_from_baseline"
    } else {
        "matches_baseline"
    };

    let _ = job_output::upsert(job_name, namespace, "_snapshot_status", status.as_bytes(), &conn);
    if let Ok(diff_json) = serde_json::to_string(&diff) {
        let _ = job_output::upsert(job_name, namespace, "_snapshot_diff_json", diff_json.as_bytes(), &conn);
    }
    if let Some(id) = baseline_id {
        let _ = job_output::upsert(job_name, namespace, "_snapshot_baseline_id", id.to_string().as_bytes(), &conn);
    }
}
