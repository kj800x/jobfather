use actix_web::{get, post, web, HttpResponse, Responder};
use k8s_openapi::api::batch::v1::Job;
use kube::api::PostParams;
use kube::{Api, Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};

use crate::db::ArchivedJob;
use crate::kubernetes::JobTemplate;

// --- API response types ---

#[derive(Serialize)]
pub(crate) struct ApiJobTemplate {
    pub name: String,
    pub namespace: String,
    pub schedule: Option<String>,
    pub cleanup_after: String,
    pub acceptance_test: bool,
    pub artifact_sha: Option<String>,
    pub config_sha: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ApiJob {
    pub name: String,
    pub namespace: String,
    pub source: String,
    pub status: String,
    pub start_time: Option<String>,
    pub completion_time: Option<String>,
    pub duration_seconds: Option<i64>,
    pub job_template_name: Option<String>,
    pub job_template_namespace: Option<String>,
    pub snapshot_status: Option<String>,
    pub artifact_sha: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ApiTestSuites {
    total_tests: u32,
    total_failures: u32,
    total_errors: u32,
    total_skipped: u32,
    all_passed: bool,
    suites: Vec<ApiTestSuite>,
}

#[derive(Serialize)]
struct ApiTestSuite {
    name: String,
    tests: u32,
    failures: u32,
    errors: u32,
    skipped: u32,
    time: Option<String>,
    cases: Vec<ApiTestCase>,
}

#[derive(Serialize)]
struct ApiTestCase {
    name: String,
    classname: Option<String>,
    time: Option<String>,
    status: String,
    message: Option<String>,
    body: Option<String>,
}

#[derive(Deserialize)]
struct RunJobRequest {
    args: Option<Vec<String>>,
    env: Option<std::collections::HashMap<String, String>>,
}

#[derive(Deserialize)]
struct LogsQuery {
    tail: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct SnapshotDiffResponse {
    #[serde(flatten)]
    diff: crate::snapshot::SnapshotDiff,
    json_diffs: std::collections::HashMap<String, Vec<crate::snapshot::DiffLine>>,
}

// --- Conversion helpers ---

pub(crate) fn jt_to_api(jt: &JobTemplate) -> ApiJobTemplate {
    let annotations = jt.metadata.annotations.as_ref();
    ApiJobTemplate {
        name: jt.name_any(),
        namespace: jt.namespace().unwrap_or_default(),
        schedule: jt.spec.schedule.clone(),
        cleanup_after: jt.cleanup_after().to_string(),
        acceptance_test: jt.is_acceptance_test(),
        artifact_sha: annotations.and_then(|a| a.get("artifactSha")).cloned(),
        config_sha: annotations.and_then(|a| a.get("configSha")).cloned(),
    }
}

fn live_job_to_api(job: &Job) -> ApiJob {
    let status = job_status(job);
    let jt_ref = job
        .metadata
        .owner_references
        .as_ref()
        .and_then(|refs| refs.iter().find(|r| r.kind == "JobTemplate"));

    let start_time = job
        .status
        .as_ref()
        .and_then(|s| s.start_time.as_ref())
        .map(|t| t.0.to_rfc3339());
    let completion_time = job
        .status
        .as_ref()
        .and_then(|s| s.completion_time.as_ref())
        .map(|t| t.0.to_rfc3339());

    let duration_seconds = job.status.as_ref().and_then(|s| {
        let start = s.start_time.as_ref()?.0;
        let end = s
            .completion_time
            .as_ref()
            .map(|t| t.0)
            .unwrap_or_else(chrono::Utc::now);
        Some((end - start).num_seconds())
    });

    let artifact_sha = job
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("artifactSha"))
        .cloned();

    ApiJob {
        name: job.name_any(),
        namespace: job.namespace().unwrap_or_default(),
        source: "live".to_string(),
        status: status.to_string(),
        start_time,
        completion_time,
        duration_seconds,
        job_template_name: jt_ref.map(|r| r.name.clone()),
        job_template_namespace: Some(job.namespace().unwrap_or_default()),
        snapshot_status: None,
        artifact_sha,
    }
}

fn archived_job_to_api(job: &ArchivedJob) -> ApiJob {
    ApiJob {
        name: job.name.clone(),
        namespace: job.namespace.clone(),
        source: "archived".to_string(),
        status: job.status.clone(),
        start_time: job.start_time.clone(),
        completion_time: job.completion_time.clone(),
        duration_seconds: job.duration_seconds,
        job_template_name: Some(job.job_template_name.clone()),
        job_template_namespace: Some(job.job_template_namespace.clone()),
        snapshot_status: job.snapshot_status.clone(),
        artifact_sha: job.artifact_sha.clone(),
    }
}

fn job_status(job: &Job) -> &str {
    let status = match job.status.as_ref() {
        Some(s) => s,
        None => return "Pending",
    };

    if let Some(conditions) = &status.conditions {
        for c in conditions {
            if c.type_ == "Complete" && c.status == "True" {
                return "Succeeded";
            }
            if c.type_ == "Failed" && c.status == "True" {
                return "Failed";
            }
        }
    }

    if status.active.is_some_and(|a| a > 0) {
        if status.ready.is_some_and(|r| r > 0) {
            return "Running";
        }
        return "Pending";
    }

    "Pending"
}

fn is_terminal(job: &Job) -> bool {
    let status = match job.status.as_ref() {
        Some(s) => s,
        None => return false,
    };
    if status.completion_time.is_some() {
        return true;
    }
    if let Some(conditions) = &status.conditions {
        for c in conditions {
            if c.type_ == "Failed" && c.status == "True" {
                return true;
            }
        }
    }
    false
}

fn junit_to_api(suites: &super::junit::TestSuites) -> ApiTestSuites {
    ApiTestSuites {
        total_tests: suites.total_tests(),
        total_failures: suites.total_failures(),
        total_errors: suites.total_errors(),
        total_skipped: suites.total_skipped(),
        all_passed: suites.all_passed(),
        suites: suites.suites.iter().map(suite_to_api).collect(),
    }
}

fn suite_to_api(suite: &super::junit::TestSuite) -> ApiTestSuite {
    ApiTestSuite {
        name: suite.name.clone(),
        tests: suite.tests,
        failures: suite.failures,
        errors: suite.errors,
        skipped: suite.skipped,
        time: suite.time.clone(),
        cases: suite.cases.iter().map(case_to_api).collect(),
    }
}

fn case_to_api(tc: &super::junit::TestCase) -> ApiTestCase {
    let (status, message, body) = match &tc.status {
        super::junit::TestCaseStatus::Passed => ("passed", None, None),
        super::junit::TestCaseStatus::Failed { message, body } => {
            ("failed", message.as_deref(), body.as_deref())
        }
        super::junit::TestCaseStatus::Error { message, body } => {
            ("error", message.as_deref(), body.as_deref())
        }
        super::junit::TestCaseStatus::Skipped { message } => {
            ("skipped", message.as_deref(), None)
        }
    };
    ApiTestCase {
        name: tc.name.clone(),
        classname: tc.classname.clone(),
        time: tc.time.clone(),
        status: status.to_string(),
        message: message.map(str::to_string),
        body: body.map(str::to_string),
    }
}

// --- Inner functions (shared by HTTP handlers and MCP) ---

pub(crate) async fn list_jobs_for_template_inner(
    namespace: &str,
    name: &str,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
) -> Vec<ApiJob> {
    let jt_api: Api<JobTemplate> = Api::namespaced(client.clone(), namespace);
    let jt_uid = jt_api.get(name).await.ok().and_then(|jt| jt.uid());

    let mut jobs: Vec<ApiJob> = Vec::new();
    if let Some(ref uid) = jt_uid {
        let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
        if let Ok(list) = job_api.list(&Default::default()).await {
            for job in &list.items {
                let owned = job
                    .metadata
                    .owner_references
                    .as_ref()
                    .is_some_and(|refs| refs.iter().any(|r| r.uid == *uid));
                if owned {
                    let mut api_job = live_job_to_api(job);
                    if is_terminal(job) {
                        if let Ok(conn) = pool.get() {
                            api_job.snapshot_status = crate::db::job_output::get_string(
                                &api_job.name,
                                namespace,
                                "_snapshot_status",
                                &conn,
                            );
                        }
                    }
                    jobs.push(api_job);
                }
            }
        }
    }

    if let Ok(conn) = pool.get() {
        if let Ok(archived) = ArchivedJob::get_by_job_template(name, namespace, &conn) {
            for job in &archived {
                jobs.push(archived_job_to_api(job));
            }
        }
    }

    jobs.sort_by(|a, b| b.start_time.cmp(&a.start_time));
    jobs
}

pub(crate) async fn get_job_inner(
    namespace: &str,
    name: &str,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
) -> Option<ApiJob> {
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
    if let Ok(job) = job_api.get(name).await {
        let mut api_job = live_job_to_api(&job);
        if is_terminal(&job) {
            if let Ok(conn) = pool.get() {
                api_job.snapshot_status =
                    crate::db::job_output::get_string(name, namespace, "_snapshot_status", &conn);
            }
        }
        return Some(api_job);
    }

    if let Ok(conn) = pool.get() {
        if let Ok(Some(job)) = ArchivedJob::get_by_name_and_namespace(name, namespace, &conn) {
            return Some(archived_job_to_api(&job));
        }
    }

    None
}

pub(crate) async fn get_job_logs_inner(
    namespace: &str,
    name: &str,
    tail: Option<i64>,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
) -> Result<String, String> {
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
    if job_api.get(name).await.is_ok() {
        return Ok(fetch_live_logs_with_tail(client, namespace, name, tail)
            .await
            .unwrap_or_default());
    }

    if let Ok(conn) = pool.get() {
        if let Ok(Some(job)) = ArchivedJob::get_by_name_and_namespace(name, namespace, &conn) {
            let text = job.logs.unwrap_or_default();
            return Ok(match tail {
                Some(n) if n > 0 => {
                    let lines: Vec<&str> = text.lines().collect();
                    let start = lines.len().saturating_sub(n as usize);
                    lines[start..].join("\n")
                }
                _ => text,
            });
        }
    }

    Err(format!("Job {}/{} not found", namespace, name))
}

pub(crate) async fn get_job_events_inner(
    namespace: &str,
    name: &str,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
) -> Vec<crate::kubernetes::events::EventInfo> {
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
    if let Ok(job) = job_api.get(name).await {
        return crate::kubernetes::events::fetch_job_events(client, namespace, &job).await;
    }

    if let Ok(conn) = pool.get() {
        if let Ok(Some(job)) = ArchivedJob::get_by_name_and_namespace(name, namespace, &conn) {
            return job
                .events_json
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or_default();
        }
    }

    Vec::new()
}

pub(crate) fn get_test_results_inner(
    namespace: &str,
    name: &str,
    pool: &Pool<SqliteConnectionManager>,
) -> Option<ApiTestSuites> {
    let xml = get_test_results_xml(name, namespace, pool)?;
    let suites = super::junit::parse_junit_xml(&xml)?;
    Some(junit_to_api(&suites))
}

fn get_test_results_xml(
    name: &str,
    namespace: &str,
    pool: &Pool<SqliteConnectionManager>,
) -> Option<String> {
    let conn = pool.get().ok()?;

    if let Some(content) = crate::db::job_output::get(name, namespace, "test-results.xml", &conn)
        .ok()
        .flatten()
    {
        return String::from_utf8(content).ok();
    }

    ArchivedJob::get_by_name_and_namespace(name, namespace, &conn)
        .ok()
        .flatten()
        .and_then(|j| j.output_test_results_xml)
}

pub(crate) fn get_job_output_inner(
    namespace: &str,
    name: &str,
    filename: &str,
    pool: &Pool<SqliteConnectionManager>,
) -> Result<String, String> {
    if filename != "result.json" && filename != "report.md" {
        return Err("Only result.json and report.md are available".to_string());
    }

    let conn = pool.get().map_err(|_| "Database error".to_string())?;

    if let Some(content) = crate::db::job_output::get(name, namespace, filename, &conn)
        .ok()
        .flatten()
    {
        if let Ok(text) = String::from_utf8(content) {
            return Ok(text);
        }
    }

    if let Ok(Some(job)) = ArchivedJob::get_by_name_and_namespace(name, namespace, &conn) {
        let text = match filename {
            "result.json" => job.output_result_json,
            "report.md" => job.output_report_md,
            _ => None,
        };
        if let Some(text) = text {
            return Ok(text);
        }
    }

    Err("Output not found".to_string())
}

pub(crate) fn get_snapshot_diff_inner(
    namespace: &str,
    name: &str,
    pool: &Pool<SqliteConnectionManager>,
) -> Option<SnapshotDiffResponse> {
    let conn = pool.get().ok()?;

    if let Some(diff_json) =
        crate::db::job_output::get_string(name, namespace, "_snapshot_diff_json", &conn)
    {
        if let Ok(diff) = serde_json::from_str::<crate::snapshot::SnapshotDiff>(&diff_json) {
            return Some(build_snapshot_diff_response(&diff, name, namespace, pool));
        }
    }

    if let Ok(Some(job)) = ArchivedJob::get_by_name_and_namespace(name, namespace, &conn) {
        if let Some(diff_json) = &job.snapshot_diff_json {
            if let Ok(diff) = serde_json::from_str::<crate::snapshot::SnapshotDiff>(diff_json) {
                return Some(build_snapshot_diff_response(&diff, name, namespace, pool));
            }
        }
    }

    None
}

pub(crate) async fn run_job_inner(
    namespace: &str,
    name: &str,
    args: Option<Vec<String>>,
    env: Option<std::collections::HashMap<String, String>>,
    client: &Client,
) -> Result<String, String> {
    let jt_api: Api<JobTemplate> = Api::namespaced(client.clone(), namespace);
    let job_template = jt_api
        .get(name)
        .await
        .map_err(|e| format!("JobTemplate not found: {}", e))?;

    let env_vec = env.map(|m| m.into_iter().collect::<Vec<_>>());

    let job = crate::kubernetes::job_create::build_job(&job_template, args, env_vec)?;

    let job_name = job.metadata.name.clone().unwrap_or_default();
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
    job_api
        .create(&PostParams::default(), &job)
        .await
        .map_err(|e| format!("Failed to create job: {}", e))?;

    log::info!("API: Created job {} in namespace {}", job_name, namespace);
    Ok(serde_json::json!({
        "name": job_name,
        "namespace": namespace,
        "status": "created"
    })
    .to_string())
}

fn build_snapshot_diff_response(
    diff: &crate::snapshot::SnapshotDiff,
    job_name: &str,
    namespace: &str,
    pool: &Pool<SqliteConnectionManager>,
) -> SnapshotDiffResponse {
    let mut json_diffs = std::collections::HashMap::new();

    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => {
            return SnapshotDiffResponse {
                diff: diff.clone(),
                json_diffs,
            }
        }
    };

    let tarball = crate::db::job_output::get(job_name, namespace, "test-snapshots.tar.gz", &conn)
        .ok()
        .flatten()
        .or_else(|| {
            ArchivedJob::get_by_name_and_namespace(job_name, namespace, &conn)
                .ok()
                .flatten()
                .and_then(|j| j.output_test_snapshots)
        });

    if let Some(tarball) = tarball {
        if let Ok(current) = crate::snapshot::extract_tarball(&tarball) {
            let jt_ref = ArchivedJob::get_by_name_and_namespace(job_name, namespace, &conn)
                .ok()
                .flatten();

            let baseline_id = jt_ref
                .as_ref()
                .and_then(|j| j.snapshot_baseline_id)
                .or_else(|| {
                    crate::db::job_output::get_string(
                        job_name,
                        namespace,
                        "_snapshot_baseline_id",
                        &conn,
                    )
                    .and_then(|s| s.parse().ok())
                });

            let baseline = match baseline_id {
                Some(id) => crate::db::snapshot::load_baseline_files(id, &conn),
                None => {
                    if let Some(ref jt) = jt_ref {
                        crate::db::snapshot::load_latest_baseline_conn(
                            &jt.job_template_name,
                            &jt.job_template_namespace,
                            &conn,
                        )
                        .1
                    } else {
                        std::collections::BTreeMap::new()
                    }
                }
            };

            for f in &diff.files {
                if f.path.ends_with(".json")
                    && f.status == crate::snapshot::FileDiffStatus::Changed
                {
                    if let (Some(old), Some(new)) = (baseline.get(&f.path), current.get(&f.path)) {
                        if let Some(lines) = crate::snapshot::json_diff(old, new) {
                            json_diffs.insert(f.path.clone(), lines);
                        }
                    }
                }
            }
        }
    }

    SnapshotDiffResponse {
        diff: diff.clone(),
        json_diffs,
    }
}

async fn fetch_live_logs_with_tail(
    client: &Client,
    namespace: &str,
    job_name: &str,
    tail: Option<i64>,
) -> Option<String> {
    let pod_api: Api<k8s_openapi::api::core::v1::Pod> =
        Api::namespaced(client.clone(), namespace);
    let pods = pod_api
        .list(&kube::api::ListParams::default().labels(&format!("job-name={}", job_name)))
        .await
        .ok()?;

    let mut all_logs = Vec::new();
    for pod in &pods.items {
        let pod_name = pod.name_any();
        let mut params = kube::api::LogParams {
            timestamps: true,
            ..Default::default()
        };
        if let Some(n) = tail {
            if n > 0 {
                params.tail_lines = Some(n);
            }
        }
        if let Ok(log_str) = pod_api.logs(&pod_name, &params).await {
            if pods.items.len() > 1 {
                all_logs.push(format!("=== Pod: {} ===\n{}", pod_name, log_str));
            } else {
                all_logs.push(log_str);
            }
        }
    }

    if all_logs.is_empty() {
        None
    } else {
        Some(all_logs.join("\n"))
    }
}

// --- HTTP route handlers ---

#[get("/api/job-templates")]
pub async fn api_list_job_templates(client: web::Data<Client>) -> impl Responder {
    let api: Api<JobTemplate> = Api::all(client.get_ref().clone());
    match api.list(&Default::default()).await {
        Ok(list) => {
            let templates: Vec<ApiJobTemplate> = list.items.iter().map(jt_to_api).collect();
            HttpResponse::Ok().json(templates)
        }
        Err(e) => HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": format!("Failed to list JobTemplates: {}", e)})),
    }
}

#[get("/api/job-templates/{namespace}/{name}")]
pub async fn api_get_job_template(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    let api: Api<JobTemplate> = Api::namespaced(client.get_ref().clone(), &namespace);
    match api.get(&name).await {
        Ok(jt) => HttpResponse::Ok().json(jt_to_api(&jt)),
        Err(_) => HttpResponse::NotFound().json(
            serde_json::json!({"error": format!("JobTemplate {}/{} not found", namespace, name)}),
        ),
    }
}

#[get("/api/job-templates/{namespace}/{name}/jobs")]
pub async fn api_list_jobs_for_template(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    let jobs = list_jobs_for_template_inner(&namespace, &name, &client, &pool).await;
    HttpResponse::Ok().json(jobs)
}

#[get("/api/jobs/{namespace}/{name}")]
pub async fn api_get_job(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    match get_job_inner(&namespace, &name, &client, &pool).await {
        Some(job) => HttpResponse::Ok().json(job),
        None => HttpResponse::NotFound()
            .json(serde_json::json!({"error": format!("Job {}/{} not found", namespace, name)})),
    }
}

#[get("/api/jobs/{namespace}/{name}/logs")]
pub async fn api_get_job_logs(
    path: web::Path<(String, String)>,
    query: web::Query<LogsQuery>,
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    match get_job_logs_inner(&namespace, &name, query.tail, &client, &pool).await {
        Ok(text) => HttpResponse::Ok().content_type("text/plain").body(text),
        Err(e) => HttpResponse::NotFound().json(serde_json::json!({"error": e})),
    }
}

#[get("/api/jobs/{namespace}/{name}/events")]
pub async fn api_get_job_events(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    let events = get_job_events_inner(&namespace, &name, &client, &pool).await;
    HttpResponse::Ok().json(events)
}

#[get("/api/jobs/{namespace}/{name}/test-results")]
pub async fn api_get_test_results(
    path: web::Path<(String, String)>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    match get_test_results_inner(&namespace, &name, &pool) {
        Some(results) => HttpResponse::Ok().json(results),
        None => HttpResponse::NotFound()
            .json(serde_json::json!({"error": "No test results found"})),
    }
}

#[get("/api/jobs/{namespace}/{name}/output/{filename}")]
pub async fn api_get_job_output(
    path: web::Path<(String, String, String)>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name, filename) = path.into_inner();
    match get_job_output_inner(&namespace, &name, &filename, &pool) {
        Ok(text) => {
            let content_type = if filename == "result.json" {
                "application/json"
            } else {
                "text/plain"
            };
            HttpResponse::Ok().content_type(content_type).body(text)
        }
        Err(e) => HttpResponse::NotFound().json(serde_json::json!({"error": e})),
    }
}

#[get("/api/jobs/{namespace}/{name}/snapshot-diff")]
pub async fn api_get_snapshot_diff(
    path: web::Path<(String, String)>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    match get_snapshot_diff_inner(&namespace, &name, &pool) {
        Some(resp) => HttpResponse::Ok().json(resp),
        None => HttpResponse::NotFound()
            .json(serde_json::json!({"error": "No snapshot diff found"})),
    }
}

#[post("/api/job-templates/{namespace}/{name}/run")]
pub async fn api_run_job(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
    body: web::Json<RunJobRequest>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    match run_job_inner(&namespace, &name, body.args.clone(), body.env.clone(), &client).await {
        Ok(text) => HttpResponse::Ok()
            .content_type("application/json")
            .body(text),
        Err(e) => HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": e})),
    }
}
