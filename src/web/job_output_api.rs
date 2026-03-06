use actix_web::{put, web, HttpResponse, Responder};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::job_output;

#[put("/api/jobs/{namespace}/{name}/output/{filename}")]
pub async fn upload_job_output(
    path: web::Path<(String, String, String)>,
    body: web::Bytes,
    pool: web::Data<Pool<SqliteConnectionManager>>,
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
            HttpResponse::Ok().body("OK")
        }
        Err(e) => {
            log::error!("Failed to store output {}/{}/{}: {}", namespace, name, filename, e);
            HttpResponse::InternalServerError().body("Failed to store output")
        }
    }
}
