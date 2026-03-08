mod db;
mod kubernetes;
mod metrics;
mod snapshot;
mod web;

use std::sync::Arc;

use actix_web::{get, middleware, web::Data, App, HttpResponse, HttpServer, Responder};
use actix_web_opentelemetry::{RequestMetrics, RequestTracing};
use jobfather::serve_static_file;
use opentelemetry::global;
use opentelemetry_sdk::metrics::MeterProvider;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

#[get("/")]
async fn root() -> impl Responder {
    HttpResponse::Found()
        .append_header(("Location", "/job-templates"))
        .finish()
}

async fn metrics_handler(
    registry: Data<prometheus::Registry>,
    pool: Data<Pool<SqliteConnectionManager>>,
    metrics: Data<Arc<metrics::Metrics>>,
    client: Data<kube::Client>,
) -> impl Responder {
    // Update time-since gauges at scrape time for accuracy
    if let Ok(conn) = pool.get() {
        metrics
            .update_time_since_gauges(&conn, client.get_ref(), chrono::Utc::now())
            .await;
    }

    let encoder = prometheus::TextEncoder::new();
    let metric_families = registry.gather();
    match encoder.encode_to_string(&metric_families) {
        Ok(body) => HttpResponse::Ok()
            .content_type("text/plain; version=0.0.4; charset=utf-8")
            .body(body),
        Err(e) => HttpResponse::InternalServerError().body(format!("Metrics encoding error: {}", e)),
    }
}

async fn start_http(
    registry: prometheus::Registry,
    pool: Pool<SqliteConnectionManager>,
    metrics: Arc<metrics::Metrics>,
) -> Result<(), std::io::Error> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    log::info!("Starting HTTP server at http://localhost:{}", port);

    let kube_client = match kube::Client::try_default().await {
        Ok(client) => {
            log::info!("Successfully initialized Kubernetes client");
            client
        }
        Err(e) => {
            log::error!("Failed to initialize Kubernetes client: {}", e);
            return Err(std::io::Error::other(e));
        }
    };

    HttpServer::new(move || {
        App::new()
            .wrap(RequestTracing::new())
            .wrap(RequestMetrics::default())
            .route(
                "/api/metrics",
                actix_web::web::get().to(metrics_handler),
            )
            .wrap(middleware::Logger::default())
            .app_data(Data::new(registry.clone()))
            .app_data(Data::new(metrics.clone()))
            .app_data(Data::new(kube_client.clone()))
            .app_data(Data::new(pool.clone()))
            .service(root)
            .service(web::job_templates_page)
            .service(web::job_templates_fragment)
            .service(web::job_template_detail_page)
            .service(web::job_template_detail_fragment)
            .service(web::job_template_run_modal)
            .service(web::job_template_run)
            .service(web::job_detail_page)
            .service(web::job_detail_fragment)
            .service(web::job_output_archive)
            .service(web::job_output_test_snapshots)
            .service(web::accept_snapshots)
            .service(web::job_snapshot_file)
            .service(web::baseline_snapshot_file)
            .service(web::baseline_set_file)
            .service(web::upload_job_output)
            .service(web::api::api_list_job_templates)
            .service(web::api::api_get_job_template)
            .service(web::api::api_list_jobs_for_template)
            .service(web::api::api_get_job)
            .service(web::api::api_get_job_logs)
            .service(web::api::api_get_job_events)
            .service(web::api::api_get_test_results)
            .service(web::api::api_get_job_output)
            .service(web::api::api_get_snapshot_diff)
            .service(web::api::api_run_job)
            .service(web::mcp::mcp_post)
            .service(web::mcp::mcp_get)
            .service(serve_static_file!("htmx.min.js"))
            .service(serve_static_file!("idiomorph.min.js"))
            .service(serve_static_file!("idiomorph-ext.min.js"))
            .service(serve_static_file!("styles.css"))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[actix_web::main]
#[allow(clippy::expect_used)]
async fn main() -> std::io::Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .filter_module("kube_runtime::controller", log::LevelFilter::Warn)
        .filter_module("actix_web::middleware::logger", log::LevelFilter::Warn)
        .parse_default_env()
        .init();

    let registry = prometheus::Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()
        .expect("Failed to build OpenTelemetry Prometheus exporter");
    let provider = MeterProvider::builder().with_reader(exporter).build();
    global::set_meter_provider(provider);

    let metrics = Arc::new(metrics::Metrics::new(&registry));

    let manager = SqliteConnectionManager::file(
        std::env::var("DATABASE_PATH").unwrap_or("db.db".to_string()),
    );
    let pool = Pool::new(manager).expect("Failed to create database pool");
    {
        let conn = pool.get().expect("Failed to get database connection");
        db::migrations::migrate(conn).expect("Failed to run database migrations");
    }

    let reconciler_client = kube::Client::try_default()
        .await
        .expect("Failed to initialize Kubernetes client for reconciler");
    let scheduler_client = reconciler_client.clone();

    tokio::select! {
        result = start_http(registry, pool.clone(), metrics.clone()) => {
            if let Err(e) = result {
                log::error!("HTTP server error: {}", e);
            }
        }
        _ = kubernetes::reconciler::run(reconciler_client, pool.clone(), metrics.clone()) => {
            log::error!("Reconciler exited unexpectedly");
        }
        _ = kubernetes::scheduler::run(scheduler_client, pool, metrics) => {
            log::error!("Scheduler exited unexpectedly");
        }
    };

    Ok(())
}
