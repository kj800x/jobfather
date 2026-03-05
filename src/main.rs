mod db;
mod kubernetes;
mod web;

use actix_web::{get, middleware, web::Data, App, HttpResponse, HttpServer, Responder};
use actix_web_opentelemetry::{PrometheusMetricsHandler, RequestMetrics, RequestTracing};
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

async fn start_http(
    registry: prometheus::Registry,
    pool: Pool<SqliteConnectionManager>,
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
            return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
        }
    };

    HttpServer::new(move || {
        App::new()
            .wrap(RequestTracing::new())
            .wrap(RequestMetrics::default())
            .route(
                "/api/metrics",
                actix_web::web::get().to(PrometheusMetricsHandler::new(registry.clone())),
            )
            .wrap(middleware::Logger::default())
            .app_data(Data::new(kube_client.clone()))
            .app_data(Data::new(pool.clone()))
            .service(root)
            .service(web::job_templates_page)
            .service(web::job_templates_fragment)
            .service(web::job_template_detail_page)
            .service(web::job_template_detail_fragment)
            .service(web::job_template_run_modal)
            .service(web::job_template_run)
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
        .parse_default_env()
        .init();

    let registry = prometheus::Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()
        .expect("Failed to build OpenTelemetry Prometheus exporter");
    let provider = MeterProvider::builder().with_reader(exporter).build();
    global::set_meter_provider(provider);

    let manager = SqliteConnectionManager::file(
        std::env::var("DATABASE_PATH").unwrap_or("db.db".to_string()),
    );
    let pool = Pool::new(manager).expect("Failed to create database pool");
    {
        let conn = pool.get().expect("Failed to get database connection");
        db::migrations::migrate(conn).expect("Failed to run database migrations");
    }

    tokio::select! {
        result = start_http(registry, pool) => {
            if let Err(e) = result {
                log::error!("HTTP server error: {}", e);
            }
        }
    };

    Ok(())
}
