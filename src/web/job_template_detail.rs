use actix_web::{HttpResponse, Responder, get, post, web};
use chrono::{DateTime, Utc};
use chrono_tz::US::Eastern;
use k8s_openapi::api::batch::v1::Job;
use kube::api::PostParams;
use kube::{Api, Client, ResourceExt};
use maud::{DOCTYPE, Markup, html};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::Deserialize;

const DEFAULT_JOB_LIMIT: usize = 100;

/// Env vars with this prefix are injected by CI/CD and shown read-only in the run dialog.
const CICD_ENV_PREFIX: &str = "CICD_";

use crate::db::ArchivedJob;
use crate::kubernetes::JobTemplate;

/// A unified row for the combined jobs table.
struct JobRow {
    name: String,
    namespace: String,
    is_live: bool,
    status: String,
    status_badge_class: String,
    start_time: Option<DateTime<Utc>>,
    duration_display: String,
    test_results: Option<TestResultSummary>,
    snapshot_status: Option<String>,
    artifact_sha: Option<String>,
}

struct TestResultSummary {
    all_passed: bool,
    failures: u32,
}

#[get("/job-templates/{namespace}/{name}")]
pub async fn job_template_detail_page(path: web::Path<(String, String)>) -> impl Responder {
    let (namespace, name) = path.into_inner();

    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                title { (name) " - Job Template" }
                (crate::web::header::stylesheet_link())
                (crate::web::header::scripts())
            }
            body hx-ext="morph" {
                (crate::web::header::render("job-templates"))
                div class="content" {
                    div class="detail-header" {
                        a href="/job-templates" class="back-link" { "Job Templates" }
                        " / "
                        span class="detail-title" { (namespace) "/" (name) }
                    }
                    div class="detail-content"
                        hx-get=(format!("/job-templates/{}/{}/fragment?limit={}", namespace, name, DEFAULT_JOB_LIMIT))
                        hx-trigger="load, every 5s"
                        hx-swap="morph:innerHTML" {
                        div { "Loading..." }
                    }
                    div id="modal-container" {}
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

#[derive(Deserialize)]
pub struct FragmentQuery {
    limit: Option<usize>,
}

#[get("/job-templates/{namespace}/{name}/fragment")]
pub async fn job_template_detail_fragment(
    path: web::Path<(String, String)>,
    query: web::Query<FragmentQuery>,
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    let limit = query.limit.unwrap_or(DEFAULT_JOB_LIMIT);

    // Fetch the JobTemplate itself
    let jt_api: Api<JobTemplate> = Api::namespaced(client.get_ref().clone(), &namespace);
    let job_template = match jt_api.get(&name).await {
        Ok(jt) => jt,
        Err(e) => {
            log::error!("Failed to get JobTemplate {}/{}: {}", namespace, name, e);
            return HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
                .body(format!(
                    "JobTemplate {}/{} not found: {}",
                    namespace, name, e
                ));
        }
    };

    let is_acceptance_test = job_template.is_acceptance_test();

    // Fetch live Jobs in the namespace, filter to ones owned by this JobTemplate
    let job_api: Api<Job> = Api::namespaced(client.get_ref().clone(), &namespace);
    let jt_uid = job_template.uid().unwrap_or_default();
    let live_jobs: Vec<Job> = match job_api.list(&Default::default()).await {
        Ok(list) => list
            .items
            .into_iter()
            .filter(|job| {
                job.metadata
                    .owner_references
                    .as_ref()
                    .is_some_and(|refs| refs.iter().any(|r| r.uid == jt_uid))
            })
            .collect(),
        Err(e) => {
            log::warn!("Failed to list Jobs in {}: {}", namespace, e);
            vec![]
        }
    };

    // Fetch archived jobs from the database
    let archived_jobs = match pool.get() {
        Ok(conn) => {
            ArchivedJob::get_by_job_template(&name, &namespace, &conn).unwrap_or_else(|e| {
                log::error!("Failed to query archived jobs: {}", e);
                vec![]
            })
        }
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            vec![]
        }
    };

    // Build unified rows
    let mut rows: Vec<JobRow> = Vec::new();

    // Live jobs
    for job in &live_jobs {
        let (badge_class, status_text) = job_status(job);
        let start_dt = job
            .status
            .as_ref()
            .and_then(|s| s.start_time.as_ref())
            .map(|t| t.0);

        let job_name_str = job.name_any();
        let is_term = is_terminal(job);

        // Load test results and snapshot status for terminal live jobs
        let (test_results, snapshot_status) = if is_acceptance_test && is_term {
            let tr = load_live_test_results(&job_name_str, &namespace, &pool);
            let ss = load_live_snapshot_status(&job_name_str, &namespace, &pool);
            (tr, ss)
        } else {
            (None, None)
        };

        let artifact_sha = job
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("artifactSha"))
            .cloned();

        rows.push(JobRow {
            name: job_name_str,
            namespace: namespace.clone(),
            is_live: true,
            status: status_text.to_string(),
            status_badge_class: badge_class.to_string(),
            start_time: start_dt,
            duration_display: job_duration_display(job),
            test_results,
            snapshot_status,
            artifact_sha,
        });
    }

    // Archived jobs
    for job in &archived_jobs {
        let badge_class = match job.status.as_str() {
            "Succeeded" => "success",
            "Failed" => "danger",
            "Running" => "info",
            _ => "muted",
        };

        let start_dt = job
            .start_time
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let test_results = if is_acceptance_test {
            job.output_test_results_xml
                .as_deref()
                .and_then(super::junit::parse_junit_xml)
                .map(|suites| TestResultSummary {
                    all_passed: suites.all_passed(),
                    failures: suites.total_failures() + suites.total_errors(),
                })
        } else {
            None
        };

        let snapshot_status = if is_acceptance_test {
            job.snapshot_status.clone()
        } else {
            None
        };

        rows.push(JobRow {
            name: job.name.clone(),
            namespace: job.namespace.clone(),
            is_live: false,
            status: job.status.clone(),
            status_badge_class: badge_class.to_string(),
            start_time: start_dt,
            duration_display: job
                .duration_seconds
                .map(format_duration)
                .unwrap_or_default(),
            test_results,
            snapshot_status,
            artifact_sha: job.artifact_sha.clone(),
        });
    }

    // Sort by start time, newest first
    rows.sort_by(|a, b| b.start_time.cmp(&a.start_time));

    let has_more = rows.len() > limit;
    rows.truncate(limit);

    let jt_annotations = job_template.metadata.annotations.as_ref();
    let jt_artifact_sha = jt_annotations
        .and_then(|a| a.get("artifactSha"))
        .map(|s| s.as_str());
    let jt_config_sha = jt_annotations
        .and_then(|a| a.get("configSha"))
        .map(|s| s.as_str());

    let markup = html! {
        div class="info-card" {
            div class="info-card-hero" {
                div class="info-hero-left" {
                    span class="info-hero-title" { (&name) }
                    @if let Some(schedule) = &job_template.spec.schedule {
                        span class="badge badge-info" { (schedule) }
                    } @else {
                        span class="badge badge-muted" { "On-demand" }
                    }
                }
                div class="info-hero-actions" {
                    button class="btn btn-primary"
                        hx-get=(format!("/job-templates/{}/{}/run-modal", namespace, name))
                        hx-target="#modal-container"
                        hx-swap="innerHTML" {
                        "Run Job"
                    }
                }
            }
            div class="info-card-grid" {
                div class="info-tile" {
                    span class="info-tile-label" { "Cleanup After" }
                    span class="info-tile-value" {
                        code { (job_template.cleanup_after()) }
                    }
                }
                div class="info-tile" {
                    span class="info-tile-label" { "Acceptance Test" }
                    span class="info-tile-value" {
                        @if is_acceptance_test {
                            span class="badge badge-info" { "Yes" }
                        } @else {
                            span class="text-muted" { "No" }
                        }
                    }
                }
                @if let Some(sha) = jt_artifact_sha {
                    div class="info-tile" {
                        span class="info-tile-label" { "Artifact SHA" }
                        span class="info-tile-value" {
                            code class="sha-badge" title=(sha) { (&sha[..sha.len().min(8)]) }
                        }
                    }
                }
                @if let Some(sha) = jt_config_sha {
                    div class="info-tile" {
                        span class="info-tile-label" { "Config SHA" }
                        span class="info-tile-value" {
                            code class="sha-badge" title=(sha) { (&sha[..sha.len().min(8)]) }
                        }
                    }
                }
            }
        }

        h3 class="section-title" { "Jobs" }
        (render_combined_table(&rows, is_acceptance_test, &name))
        @if has_more {
            @let next_limit = limit + DEFAULT_JOB_LIMIT;
            @let fragment_url = format!("/job-templates/{}/{}/fragment?limit={}", namespace, name, next_limit);
            div class="load-more-container" {
                button class="btn btn-secondary"
                    onclick=(format!(
                        "let c=this.closest('.detail-content'); c.setAttribute('hx-get','{}'); htmx.trigger(c,'load')",
                        fragment_url
                    )) {
                    "Load more"
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

fn render_combined_table(rows: &[JobRow], is_acceptance_test: bool, jt_name: &str) -> Markup {
    let has_any_sha = rows.iter().any(|r| r.artifact_sha.is_some());
    let col_count = 5 + if is_acceptance_test { 2 } else { 0 } + if has_any_sha { 1 } else { 0 };

    // Mark rows where the artifact SHA changed from the previous (next in time) row
    let sha_changed: Vec<bool> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let prev_sha = rows.get(i + 1).and_then(|r| r.artifact_sha.as_deref());
            match (&row.artifact_sha, prev_sha) {
                (Some(cur), Some(prev)) => cur != prev,
                _ => false,
            }
        })
        .collect();

    html! {
        table class="jt-table" {
            thead {
                tr {
                    th { "Name" }
                    @if has_any_sha {
                        th class="badge-col badge-col-wide" { "Artifact" }
                    }
                    th class="badge-col badge-col-wide" { "Source" }
                    th class="badge-col badge-col-wide" { "Status" }
                    @if is_acceptance_test {
                        th class="badge-col badge-col-wide" { "Tests" }
                        th class="badge-col badge-col-wide" { "Snapshots" }
                    }
                    th class="badge-col badge-col-narrow" { "Info" }
                    th class="time-cell" { "Started" }
                    th class="time-cell duration-col" { "Duration" }
                }
            }
            tbody {
                @if rows.is_empty() {
                    tr {
                        td colspan=(col_count) class="empty-state" {
                            "No jobs yet."
                        }
                    }
                }
                @for (i, row) in rows.iter().enumerate() {
                    tr {
                        td class="jt-name" {
                            @let display_name = row.name.strip_prefix(jt_name).and_then(|s| s.strip_prefix('-')).unwrap_or(&row.name);
                            a href=(format!("/jobs/{}/{}", row.namespace, row.name)) { (display_name) }
                        }
                        @if has_any_sha {
                            td class="badge-col badge-col-wide" {
                                @if let Some(ref sha) = row.artifact_sha {
                                    @let class = if sha_changed[i] { "sha-badge sha-changed" } else { "sha-badge" };
                                    code class=(class) title=(sha) data-sha=(sha) { (&sha[..sha.len().min(8)]) }
                                }
                            }
                        }
                        td class="badge-col badge-col-wide" {
                            @if row.is_live {
                                span class="badge badge-info" { "Live" }
                            } @else {
                                span class="badge badge-muted" { "Archived" }
                            }
                        }
                        td class="badge-col badge-col-wide" {
                            span class=(format!("badge badge-{}", row.status_badge_class)) { (&row.status) }
                        }
                        @if is_acceptance_test {
                            td class="badge-col badge-col-wide" {
                                @if let Some(ref tr) = row.test_results {
                                    @if tr.all_passed {
                                        span class="badge badge-success" { "Passing" }
                                    } @else {
                                        span class="badge badge-danger" { (tr.failures) " Failing" }
                                    }
                                }
                            }
                            td class="badge-col badge-col-wide" {
                                @if let Some(ref status) = row.snapshot_status {
                                    @match status.as_str() {
                                        "matches_baseline" => {
                                            span class="badge badge-success" { "Matches baseline" }
                                        },
                                        "accepted_as_baseline" => {
                                            span class="badge badge-info" { "Accepted as new baseline" }
                                        },
                                        "differs_from_baseline" => {
                                            span class="badge badge-danger" { "Differs from baseline" }
                                        },
                                        _ => {
                                            span class="badge badge-muted" { (status) }
                                        },
                                    }
                                }
                            }
                        }
                        // Combined badges column for narrow screens
                        td class="badge-col badge-col-narrow" {
                            @if let Some(ref sha) = row.artifact_sha {
                                @let class = if sha_changed[i] { "sha-badge sha-changed" } else { "sha-badge" };
                                code class=(class) title=(sha) data-sha=(sha) { (&sha[..sha.len().min(8)]) }
                                " "
                            }
                            @if row.is_live {
                                span class="badge badge-info" { "Live" }
                            } @else {
                                span class="badge badge-muted" { "Archived" }
                            }
                            " "
                            span class=(format!("badge badge-{}", row.status_badge_class)) { (&row.status) }
                            @if is_acceptance_test {
                                @if let Some(ref tr) = row.test_results {
                                    " "
                                    @if tr.all_passed {
                                        span class="badge badge-success" { "Passing" }
                                    } @else {
                                        span class="badge badge-danger" { (tr.failures) " Failing" }
                                    }
                                }
                                @if let Some(ref status) = row.snapshot_status {
                                    " "
                                    @match status.as_str() {
                                        "matches_baseline" => {
                                            span class="badge badge-success" { "Matches baseline" }
                                        },
                                        "accepted_as_baseline" => {
                                            span class="badge badge-info" { "Accepted as new baseline" }
                                        },
                                        "differs_from_baseline" => {
                                            span class="badge badge-danger" { "Differs from baseline" }
                                        },
                                        _ => {
                                            span class="badge badge-muted" { (status) }
                                        },
                                    }
                                }
                            }
                        }
                        td class="time-cell" {
                            @if let Some(dt) = row.start_time {
                                span title=(format_eastern(&dt)) { (timeago(&dt)) }
                            }
                        }
                        td class="time-cell duration-col" {
                            (&row.duration_display)
                        }
                    }
                }
            }
        }
    }
}

fn load_live_test_results(
    job_name: &str,
    namespace: &str,
    pool: &Pool<SqliteConnectionManager>,
) -> Option<TestResultSummary> {
    let conn = pool.get().ok()?;
    let content = crate::db::job_output::get(job_name, namespace, "test-results.xml", &conn)
        .ok()
        .flatten()?;
    let xml = String::from_utf8(content).ok()?;
    let suites = super::junit::parse_junit_xml(&xml)?;
    Some(TestResultSummary {
        all_passed: suites.all_passed(),
        failures: suites.total_failures() + suites.total_errors(),
    })
}

fn load_live_snapshot_status(
    job_name: &str,
    namespace: &str,
    pool: &Pool<SqliteConnectionManager>,
) -> Option<String> {
    let conn = pool.get().ok()?;
    crate::db::job_output::get_string(job_name, namespace, "_snapshot_status", &conn)
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

/// Format a UTC datetime as timeago (e.g. "just now", "5 minutes ago", "2 days ago").
fn timeago(dt: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now.signed_duration_since(*dt);
    let secs = delta.num_seconds();

    if secs < 0 {
        return "just now".to_string();
    }
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins == 1 {
        return "1 minute ago".to_string();
    }
    if mins < 60 {
        return format!("{} minutes ago", mins);
    }
    let hours = mins / 60;
    if hours == 1 {
        return "1 hour ago".to_string();
    }
    if hours < 24 {
        return format!("{} hours ago", hours);
    }
    let days = hours / 24;
    if days == 1 {
        return "1 day ago".to_string();
    }
    format!("{} days ago", days)
}

/// Format a UTC datetime as human-readable Eastern time for hover tooltips.
/// e.g. "March 6th, 2026 9:02:00 PM Eastern"
fn format_eastern(dt: &DateTime<Utc>) -> String {
    let eastern = dt.with_timezone(&Eastern);
    let month = eastern.format("%B").to_string();
    let day = eastern
        .format("%-d")
        .to_string()
        .parse::<u32>()
        .unwrap_or(1);
    let suffix = ordinal_suffix(day);
    let rest = eastern.format("%-I:%M:%S %p").to_string();
    let year = eastern.format("%Y").to_string();
    format!("{} {}{}, {} {} Eastern", month, day, suffix, year, rest)
}

fn ordinal_suffix(n: u32) -> &'static str {
    match (n % 10, n % 100) {
        (1, 11) => "th",
        (2, 12) => "th",
        (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

/// Everything the run-job modal needs to render itself.
///
/// Built from the JobTemplate for the initial GET, and rebuilt from the
/// JobTemplate plus the submitted form when a POST bounces back with an error,
/// so the user never loses what they typed.
struct RunModalData {
    namespace: String,
    name: String,
    image: String,
    /// The container's command as a shell-quoted line, or None when the template
    /// leaves the image's entrypoint alone.
    command_line: Option<String>,
    /// Plain-value env vars the user may edit. Names come from the template.
    editable: Vec<(String, String)>,
    /// CICD_* env vars: shown, submitted, but not editable.
    locked: Vec<(String, String)>,
    /// valueFrom env vars: shown only. build_job carries these over untouched.
    refs: Vec<(String, String)>,
    /// Env vars the user added in the modal (name and value both editable).
    extra: Vec<(String, String)>,
    /// Arguments as a single shell-quoted line.
    args_line: String,
    command_error: Option<String>,
    args_error: Option<String>,
}

impl RunModalData {
    fn from_template(job_template: &JobTemplate, namespace: &str, name: &str) -> Self {
        let first_container = job_template
            .spec
            .spec
            .get("containers")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());

        let string_list = |key: &str| -> Vec<String> {
            first_container
                .and_then(|c| c.get(key))
                .and_then(|a| a.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|a| a.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut editable: Vec<(String, String)> = Vec::new();
        let mut locked: Vec<(String, String)> = Vec::new();
        let mut refs: Vec<(String, String)> = Vec::new();

        if let Some(envs) = first_container
            .and_then(|c| c.get("env"))
            .and_then(|e| e.as_array())
        {
            for e in envs {
                let Some(env_name) = e.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                if let Some(value_from) = e.get("valueFrom") {
                    refs.push((env_name.to_string(), describe_value_from(value_from)));
                } else {
                    let value = e.get("value").and_then(|v| v.as_str()).unwrap_or("");
                    if env_name.starts_with(CICD_ENV_PREFIX) {
                        locked.push((env_name.to_string(), value.to_string()));
                    } else {
                        editable.push((env_name.to_string(), value.to_string()));
                    }
                }
            }
        }

        Self {
            namespace: namespace.to_string(),
            name: name.to_string(),
            image: first_container
                .and_then(|c| c.get("image"))
                .and_then(|i| i.as_str())
                .unwrap_or("unknown")
                .to_string(),
            command_line: match string_list("command") {
                command if command.is_empty() => None,
                command => Some(shell_words::join(command)),
            },
            args_line: shell_words::join(string_list("args")),
            editable,
            locked,
            refs,
            extra: Vec::new(),
            command_error: None,
            args_error: None,
        }
    }

    /// Overlay what the user submitted on top of the template's values, so a
    /// re-rendered modal shows their edits rather than resetting them.
    fn apply_submission(
        &mut self,
        command_line: Option<String>,
        args_line: String,
        submitted_env: Vec<(String, String)>,
    ) {
        if self.command_line.is_some() {
            self.command_line = command_line;
        }
        self.args_line = args_line;
        for (env_name, value) in submitted_env {
            if env_name.starts_with(CICD_ENV_PREFIX) {
                continue; // not editable; the template is the source of truth
            }
            match self.editable.iter_mut().find(|(n, _)| *n == env_name) {
                Some(entry) => entry.1 = value,
                None => self.extra.push((env_name, value)),
            }
        }
    }

    fn env_count(&self) -> usize {
        self.editable.len() + self.locked.len() + self.refs.len() + self.extra.len()
    }
}

fn field_class(has_error: bool) -> &'static str {
    if has_error {
        "args-field args-field-error"
    } else {
        "args-field"
    }
}

/// Renders the run-job modal. Read-only configuration lives inside a collapsed
/// `<details>` so the arguments field is what you see on open.
fn render_run_modal(data: &RunModalData) -> Markup {
    // Form field indices run contiguously across editable, locked and added rows.
    let locked_offset = data.editable.len();
    let extra_offset = locked_offset + data.locked.len();
    let next_index = extra_offset + data.extra.len();
    let env_count = data.env_count();

    html! {
        dialog id="run-job-dialog" class="modal" open {
            div class="modal-backdrop" onclick="this.closest('dialog').remove()" {}
            div class="modal-content" {
                div class="modal-header" {
                    div class="modal-title-group" {
                        span class="modal-title" { "Run job" }
                        span class="modal-subtitle" { (data.namespace) " / " (data.name) }
                        span class="modal-subtitle-dim" title=(data.image) { (data.image) }
                    }
                    button class="modal-close" onclick="this.closest('dialog').remove()" { "x" }
                }
                form
                    hx-post=(format!("/job-templates/{}/{}/run", data.namespace, data.name))
                    hx-target="#modal-container"
                    hx-swap="innerHTML" {
                    div class="modal-body" {
                        @if let Some(command_line) = &data.command_line {
                            div class="form-group" {
                                label for="command" { "Command" }
                                div class=(field_class(data.command_error.is_some())) {
                                    input type="text" id="command" name="command" value=(command_line)
                                        class="args-input" placeholder="entrypoint"
                                        autocapitalize="off" autocorrect="off" spellcheck="false";
                                }
                                @if let Some(error) = &data.command_error {
                                    div class="form-error" { (error) }
                                }
                            }
                        }
                        div class="form-group" {
                            label for="args" { "Arguments" }
                            div class=(field_class(data.args_error.is_some())) {
                                input type="text" id="args" name="args" value=(data.args_line)
                                    class="args-input" placeholder="--flag value"
                                    autocapitalize="off" autocorrect="off" spellcheck="false";
                            }
                            @if let Some(error) = &data.args_error {
                                div class="form-error" { (error) }
                            }
                        }

                        details class="env-details" {
                            summary class="env-summary" {
                                span class="env-summary-title" { "Environment" }
                                span class="env-summary-count" {
                                    (env_count) " variables from the template"
                                }
                            }
                            div class="env-details-body" {
                                @for (i, (env_name, value)) in data.editable.iter().enumerate() {
                                    div class="env-edit-row" {
                                        span class="env-key" { (env_name) }
                                        input type="hidden" name=(format!("env_name_{}", i)) value=(env_name);
                                        input type="text" name=(format!("env_value_{}", i)) value=(value)
                                            class="env-value" placeholder="value"
                                            autocapitalize="off" autocorrect="off" spellcheck="false";
                                    }
                                }
                                @for (i, (env_name, value)) in data.extra.iter().enumerate() {
                                    div class="env-edit-row" {
                                        input type="text" name=(format!("env_name_{}", extra_offset + i)) value=(env_name)
                                            class="env-key-input" placeholder="NAME"
                                            autocapitalize="off" autocorrect="off" spellcheck="false";
                                        input type="text" name=(format!("env_value_{}", extra_offset + i)) value=(value)
                                            class="env-value" placeholder="value"
                                            autocapitalize="off" autocorrect="off" spellcheck="false";
                                    }
                                }

                                @if !data.locked.is_empty() || !data.refs.is_empty() {
                                    div class="env-divider" {
                                        span class="env-divider-label" { "Set by CI/CD and secrets" }
                                    }
                                    @for (env_name, source) in &data.refs {
                                        div class="env-static-row" {
                                            span class="env-static-key" { (env_name) }
                                            span class="env-static-value" { (source) }
                                        }
                                    }
                                    @for (i, (env_name, value)) in data.locked.iter().enumerate() {
                                        div class="env-static-row" {
                                            span class="env-static-key" { (env_name) }
                                            span class="env-static-value" { (value) }
                                            // Not editable, but still submitted: build_job replaces
                                            // every plain-value env var with what the form sends.
                                            input type="hidden" name=(format!("env_name_{}", locked_offset + i)) value=(env_name);
                                            input type="hidden" name=(format!("env_value_{}", locked_offset + i)) value=(value);
                                        }
                                    }
                                }

                                button type="button" class="env-add-btn" data-env-add=(next_index) {
                                    "+ Add variable"
                                }
                            }
                        }
                    }
                    div class="modal-footer" {
                        button type="button" class="btn btn-secondary" onclick="this.closest('dialog').remove()" { "Cancel" }
                        button type="submit" class="btn btn-primary" { "Run job" }
                    }
                }
            }
            script { (maud::PreEscaped(ENV_ADD_SCRIPT)) }
        }
    }
}

/// Appends a blank name/value row when "+ Add variable" is clicked. Inline
/// because the modal markup is swapped in by htmx, which evaluates scripts in
/// swapped content.
const ENV_ADD_SCRIPT: &str = r#"
(function () {
  var btn = document.querySelector('#run-job-dialog [data-env-add]');
  if (!btn) return;
  var next = parseInt(btn.getAttribute('data-env-add'), 10);
  btn.addEventListener('click', function () {
    var row = document.createElement('div');
    row.className = 'env-edit-row';
    var key = document.createElement('input');
    key.type = 'text';
    key.className = 'env-key-input';
    key.placeholder = 'NAME';
    key.name = 'env_name_' + next;
    var value = document.createElement('input');
    value.type = 'text';
    value.className = 'env-value';
    value.placeholder = 'value';
    value.name = 'env_value_' + next;
    [key, value].forEach(function (el) {
      el.setAttribute('autocapitalize', 'off');
      el.setAttribute('autocorrect', 'off');
      el.setAttribute('spellcheck', 'false');
    });
    row.appendChild(key);
    row.appendChild(value);
    btn.parentNode.insertBefore(row, btn);
    next++;
    key.focus();
  });
})();
"#;

#[get("/job-templates/{namespace}/{name}/run-modal")]
pub async fn job_template_run_modal(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    let jt_api: Api<JobTemplate> = Api::namespaced(client.get_ref().clone(), &namespace);
    let job_template = match jt_api.get(&name).await {
        Ok(jt) => jt,
        Err(e) => {
            return HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
                .body(format!("JobTemplate not found: {}", e));
        }
    };

    let data = RunModalData::from_template(&job_template, &namespace, &name);
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_run_modal(&data).into_string())
}

#[derive(Deserialize)]
pub struct RunJobForm {
    command: Option<String>,
    args: Option<String>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, String>,
}

/// Collect `env_name_N` / `env_value_N` pairs, ordered by N.
fn parse_form_env(form: &RunJobForm) -> Vec<(String, String)> {
    let mut indexed: Vec<(usize, String, String)> = form
        .extra
        .iter()
        .filter_map(|(key, env_name)| {
            let index: usize = key.strip_prefix("env_name_")?.parse().ok()?;
            if env_name.is_empty() {
                return None;
            }
            let value = form
                .extra
                .get(&format!("env_value_{}", index))
                .cloned()
                .unwrap_or_default();
            Some((index, env_name.clone(), value))
        })
        .collect();

    indexed.sort_by_key(|(index, _, _)| *index);
    indexed
        .into_iter()
        .map(|(_, env_name, value)| (env_name, value))
        .collect()
}

#[post("/job-templates/{namespace}/{name}/run")]
pub async fn job_template_run(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
    form: web::Form<RunJobForm>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    let jt_api: Api<JobTemplate> = Api::namespaced(client.get_ref().clone(), &namespace);
    let job_template = match jt_api.get(&name).await {
        Ok(jt) => jt,
        Err(e) => {
            return HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
                .body(format!("JobTemplate not found: {}", e));
        }
    };

    let command_line = form.command.clone();
    let args_line = form.args.clone().unwrap_or_default();
    let form_env = parse_form_env(&form);

    // The container is exec'd without a shell, so these single-line fields are
    // split here using shell quoting rules.
    let command = command_line.as_deref().map(shell_words::split).transpose();
    let args = shell_words::split(&args_line);

    let (command, args) = match (command, args) {
        (Ok(command), Ok(args)) => (command, args),
        (command, args) => {
            let unbalanced = "Unbalanced quote.".to_string();
            let mut data = RunModalData::from_template(&job_template, &namespace, &name);
            data.apply_submission(command_line, args_line, form_env);
            data.command_error = command.err().map(|_| unbalanced.clone());
            data.args_error = args.err().map(|_| unbalanced);
            return HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(render_run_modal(&data).into_string());
        }
    };

    let job = match crate::kubernetes::job_create::build_job(
        &job_template,
        crate::kubernetes::job_create::JobOverrides {
            command,
            args: Some(args),
            env: Some(form_env),
        },
    ) {
        Ok(job) => job,
        Err(e) => {
            log::error!("Failed to build job: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body(render_modal_result(false, &e));
        }
    };

    let job_name = job.metadata.name.clone().unwrap_or_default();
    let job_api: Api<Job> = Api::namespaced(client.get_ref().clone(), &namespace);
    match job_api.create(&PostParams::default(), &job).await {
        Ok(_) => {
            log::info!("Created job {} in namespace {}", job_name, namespace);
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(render_modal_result(
                    true,
                    &format!("Job {} created", job_name),
                ))
        }
        Err(e) => {
            log::error!("Failed to create job: {}", e);
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(render_modal_result(
                    false,
                    &format!("Failed to create job: {}", e),
                ))
        }
    }
}
fn render_modal_result(success: bool, message: &str) -> String {
    let markup = html! {
        dialog id="run-job-dialog" class="modal" open {
            div class="modal-backdrop" onclick="this.closest('dialog').remove()" {}
            div class="modal-content" {
                div class="modal-header" {
                    h3 {
                        @if success { "Job Created" } @else { "Error" }
                    }
                    button class="modal-close" onclick="this.closest('dialog').remove()" { "x" }
                }
                div class="modal-body" {
                    div class=(if success { "result-message result-success" } else { "result-message result-error" }) {
                        (message)
                    }
                }
                div class="modal-footer" {
                    button type="button" class="btn btn-primary" onclick="this.closest('dialog').remove()" { "Close" }
                }
            }
        }
    };
    markup.into_string()
}

fn describe_value_from(value_from: &serde_json::Value) -> String {
    if let Some(secret) = value_from.get("secretKeyRef") {
        let name = secret.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let key = secret.get("key").and_then(|k| k.as_str()).unwrap_or("?");
        return format!("secret: {}/{}", name, key);
    }
    if let Some(cm) = value_from.get("configMapKeyRef") {
        let name = cm.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let key = cm.get("key").and_then(|k| k.as_str()).unwrap_or("?");
        return format!("configmap: {}/{}", name, key);
    }
    if value_from.get("fieldRef").is_some() {
        let path = value_from
            .get("fieldRef")
            .and_then(|f| f.get("fieldPath"))
            .and_then(|p| p.as_str())
            .unwrap_or("?");
        return format!("field: {}", path);
    }
    if value_from.get("resourceFieldRef").is_some() {
        return "resourceField".to_string();
    }
    "ref".to_string()
}

fn job_status(job: &Job) -> (&str, &str) {
    let status = match job.status.as_ref() {
        Some(s) => s,
        None => return ("muted", "Pending"),
    };

    if let Some(conditions) = &status.conditions {
        for c in conditions {
            if c.type_ == "Complete" && c.status == "True" {
                return ("success", "Succeeded");
            }
            if c.type_ == "Failed" && c.status == "True" {
                return ("danger", "Failed");
            }
        }
    }

    let has_completed_pod =
        status.succeeded.is_some_and(|n| n > 0) || status.failed.is_some_and(|n| n > 0);

    if status.active.is_some_and(|a| a > 0) {
        if status.ready.is_some_and(|r| r > 0) {
            return ("info", "Running");
        }
        if has_completed_pod {
            return ("muted", "Cleaning up");
        }
        return ("muted", "Pending");
    }

    if has_completed_pod {
        return ("muted", "Cleaning up");
    }

    ("muted", "Pending")
}

fn job_duration_display(job: &Job) -> String {
    let status = match job.status.as_ref() {
        Some(s) => s,
        None => return String::new(),
    };

    let start = match status.start_time.as_ref() {
        Some(t) => t.0,
        None => return String::new(),
    };

    let end = status
        .completion_time
        .as_ref()
        .map(|t| t.0)
        .unwrap_or_else(chrono::Utc::now);

    let seconds = (end - start).num_seconds();
    format_duration(seconds)
}

fn format_duration(seconds: i64) -> String {
    if seconds < 60 {
        format!("{}s", seconds)
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}
