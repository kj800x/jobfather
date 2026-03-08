use actix_web::{get, web, HttpResponse, Responder};
use k8s_openapi::api::batch::v1::Job;
use kube::api::LogParams;
use kube::{Api, Client, ResourceExt};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use std::collections::BTreeMap;

use crate::db::ArchivedJob;
use crate::kubernetes::events::{self, EventInfo};
use crate::kubernetes::job_output::JobOutput;
use crate::snapshot::{self, DiffLine, FileDiffStatus, SnapshotDiff};

struct SnapshotFileView {
    path: String,
    status: FileDiffStatus,
    diff_lines: Option<Vec<DiffLine>>,
}

struct SnapshotContext {
    file_views: Vec<SnapshotFileView>,
    snapshot_status: Option<String>,
    jt_name: String,
    jt_namespace: String,
    /// The baseline set ID this job compared against.
    baseline_id: Option<i64>,
    /// True if this is the latest run with snapshots for the template.
    is_latest_run: bool,
}

fn build_snapshot_file_views(
    diff: &SnapshotDiff,
    baseline: &BTreeMap<String, Vec<u8>>,
    current: &BTreeMap<String, Vec<u8>>,
) -> Vec<SnapshotFileView> {
    let mut views: Vec<SnapshotFileView> = Vec::new();

    // Add differing files
    for f in &diff.files {
        let diff_lines = if f.path.ends_with(".json") && f.status == FileDiffStatus::Changed {
            baseline
                .get(&f.path)
                .and_then(|old| current.get(&f.path).and_then(|new| snapshot::json_diff(old, new)))
        } else {
            None
        };
        views.push(SnapshotFileView {
            path: f.path.clone(),
            status: f.status.clone(),
            diff_lines,
        });
    }

    // Add matched files (in both baseline and current with same content)
    let diff_paths: std::collections::HashSet<&str> =
        diff.files.iter().map(|f| f.path.as_str()).collect();
    for path in current.keys() {
        if !diff_paths.contains(path.as_str()) {
            views.push(SnapshotFileView {
                path: path.clone(),
                status: FileDiffStatus::Matched,
                diff_lines: None,
            });
        }
    }

    views.sort_by(|a, b| a.path.cmp(&b.path));
    views
}

#[get("/jobs/{namespace}/{name}")]
pub async fn job_detail_page(
    path: web::Path<(String, String)>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                title { (name) " - Job" }
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
                        hx-get=(format!("/jobs/{}/{}/fragment", namespace, name))
                        hx-trigger="load, every 5s"
                        hx-swap="morph:innerHTML" {
                        div { "Loading..." }
                    }
                }
                script {
                    (PreEscaped(r#"
                        (function() {
                            var atBottom = true;
                            document.body.addEventListener('htmx:beforeRequest', function() {
                                var el = document.querySelector('.log-viewer');
                                if (el) {
                                    atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 16;
                                } else {
                                    atBottom = true;
                                }
                            });
                            document.body.addEventListener('htmx:afterSettle', function() {
                                var el = document.querySelector('.log-viewer');
                                if (el && atBottom) {
                                    el.scrollTop = el.scrollHeight;
                                }
                            });
                        })();
                    "#))
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

#[get("/jobs/{namespace}/{name}/fragment")]
pub async fn job_detail_fragment(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    // Try to find the job live in Kubernetes first
    let job_api: Api<Job> = Api::namespaced(client.get_ref().clone(), &namespace);
    match job_api.get(&name).await {
        Ok(job) => {
            let markup = render_live_job(&job, &client, &namespace, &pool).await;
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(markup.into_string())
        }
        Err(_) => {
            // Job not found in Kubernetes — look in the archive
            let archived = pool.get().ok().and_then(|conn| {
                ArchivedJob::get_by_name_and_namespace(&name, &namespace, &conn).ok().flatten()
            });

            match archived {
                Some(job) => {
                    let markup = render_archived_job(&job, &pool);
                    HttpResponse::Ok()
                        .content_type("text/html; charset=utf-8")
                        .body(markup.into_string())
                }
                None => {
                    HttpResponse::NotFound()
                        .content_type("text/html; charset=utf-8")
                        .body(format!("Job {}/{} not found", namespace, name))
                }
            }
        }
    }
}

#[get("/jobs/{namespace}/{name}/output/archive.tar.gz")]
pub async fn job_output_archive(
    path: web::Path<(String, String)>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    // Try job_output table first (live jobs), then archived_job
    if let Some(archive) = pool.get().ok().and_then(|conn| {
        crate::db::job_output::get(&name, &namespace, "archive.tar.gz", &conn).ok().flatten()
    }) {
        return HttpResponse::Ok()
            .content_type("application/gzip")
            .append_header(("Content-Disposition", "attachment; filename=\"archive.tar.gz\""))
            .body(archive);
    }

    if let Some(archive) = pool.get().ok().and_then(|conn| {
        ArchivedJob::get_by_name_and_namespace(&name, &namespace, &conn)
            .ok()
            .flatten()
            .and_then(|j| j.output_archive)
    }) {
        return HttpResponse::Ok()
            .content_type("application/gzip")
            .append_header(("Content-Disposition", "attachment; filename=\"archive.tar.gz\""))
            .body(archive);
    }

    HttpResponse::NotFound()
        .content_type("text/plain")
        .body("No archive found")
}

#[get("/jobs/{namespace}/{name}/output/test-snapshots.tar.gz")]
pub async fn job_output_test_snapshots(
    path: web::Path<(String, String)>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    if let Some(data) = pool.get().ok().and_then(|conn| {
        crate::db::job_output::get(&name, &namespace, "test-snapshots.tar.gz", &conn).ok().flatten()
    }) {
        return HttpResponse::Ok()
            .content_type("application/gzip")
            .append_header(("Content-Disposition", "attachment; filename=\"test-snapshots.tar.gz\""))
            .body(data);
    }

    if let Some(data) = pool.get().ok().and_then(|conn| {
        ArchivedJob::get_by_name_and_namespace(&name, &namespace, &conn)
            .ok()
            .flatten()
            .and_then(|j| j.output_test_snapshots)
    }) {
        return HttpResponse::Ok()
            .content_type("application/gzip")
            .append_header(("Content-Disposition", "attachment; filename=\"test-snapshots.tar.gz\""))
            .body(data);
    }

    HttpResponse::NotFound()
        .content_type("text/plain")
        .body("No test snapshots found")
}

async fn render_live_job(job: &Job, client: &Client, namespace: &str, pool: &Pool<SqliteConnectionManager>) -> Markup {
    let job_name = job.name_any();
    let status = job_status(job);
    let start_time = job.status.as_ref()
        .and_then(|s| s.start_time.as_ref())
        .map(|t| t.0.format("%Y-%m-%d %H:%M:%S").to_string());
    let completion_time = job.status.as_ref()
        .and_then(|s| s.completion_time.as_ref())
        .map(|t| t.0.format("%Y-%m-%d %H:%M:%S").to_string());
    let duration = job_duration_display(job);

    // Find parent JobTemplate from owner references
    let jt_ref = job.metadata.owner_references.as_ref()
        .and_then(|refs| refs.iter().find(|r| r.kind == "JobTemplate"));

    // Fetch live logs, events, and job output
    let logs = fetch_live_logs(client, namespace, &job_name).await;
    let events = events::fetch_job_events(client, namespace, job).await;

    // Load job output from DB (uploaded by sidecar)
    let output = if is_terminal(job) {
        JobOutput::load(&job_name, namespace, pool)
    } else {
        JobOutput::empty()
    };

    html! {
        div class="info-card" {
            div class="info-card-hero" {
                div class="info-hero-left" {
                    span class="info-hero-title" { (&job_name) }
                    span class=(format!("badge badge-{}", status.0)) { (status.1) }
                    span class="badge badge-info" { "Live" }
                }
                @if !duration.is_empty() {
                    div class="info-hero-duration" {
                        span class="info-hero-duration-value" { (duration) }
                    }
                }
            }
            div class="info-card-grid" {
                @if let Some(jt) = jt_ref {
                    div class="info-tile" {
                        span class="info-tile-label" { "Job Template" }
                        span class="info-tile-value" {
                            a href=(format!("/job-templates/{}/{}", namespace, jt.name)) { (&jt.name) }
                        }
                    }
                }
                div class="info-tile" {
                    span class="info-tile-label" { "Started" }
                    span class="info-tile-value" {
                        @if let Some(t) = &start_time {
                            (t)
                        } @else {
                            span class="text-muted" { "Not started" }
                        }
                    }
                }
                @if let Some(t) = &completion_time {
                    div class="info-tile" {
                        span class="info-tile-label" { "Completed" }
                        span class="info-tile-value" { (t) }
                    }
                }
            }
        }

        @let snap_ctx = output.test_snapshots.as_ref().and_then(|tarball| {
            let jt = jt_ref?;
            let current = snapshot::extract_tarball(tarball).ok()?;
            let conn = pool.get().ok()?;
            // Load the baseline this job was compared against (stored at upload time)
            let stored_baseline_id = crate::db::job_output::get_string(&job_name, namespace, "_snapshot_baseline_id", &conn)
                .and_then(|s| s.parse::<i64>().ok());
            let (baseline_id, baseline) = match stored_baseline_id {
                Some(id) => (Some(id), crate::db::snapshot::load_baseline_files(id, &conn)),
                None => crate::db::snapshot::load_latest_baseline_conn(&jt.name, namespace, &conn),
            };
            let diff = snapshot::compare(&baseline, &current);
            let file_views = build_snapshot_file_views(&diff, &baseline, &current);
            let stored_status = crate::db::job_output::get_string(&job_name, namespace, "_snapshot_status", &conn);
            let is_latest_run = crate::db::snapshot::is_latest_snapshot_job(
                &jt.name, namespace, &job_name, namespace, &conn,
            );
            Some(SnapshotContext {
                file_views,
                snapshot_status: stored_status,
                jt_name: jt.name.clone(),
                jt_namespace: namespace.to_string(),
                baseline_id,
                is_latest_run,
            })
        });

        (render_output_sections(&output, namespace, &job_name, snap_ctx.as_ref()))

        (render_events_section(&events))

        h3 class="section-title" { "Logs" }
        @if let Some(log_text) = &logs {
            pre class="log-viewer" { (log_text) }
        } @else {
            div class="empty-state-box" { "No logs available." }
        }
    }
}

fn render_archived_job(job: &ArchivedJob, pool: &Pool<SqliteConnectionManager>) -> Markup {
    let badge_class = match job.status.as_str() {
        "Succeeded" => "success",
        "Failed" => "danger",
        _ => "muted",
    };

    let output = JobOutput {
        result_json: job.output_result_json.clone(),
        report_md: job.output_report_md.clone(),
        test_results_xml: job.output_test_results_xml.clone(),
        archive: job.output_archive.clone(),
        test_snapshots: job.output_test_snapshots.clone(),
    };

    let archived_events: Vec<EventInfo> = job
        .events_json
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default();

    let duration_str = job.duration_seconds.map(format_duration);

    html! {
        div class="info-card" {
            div class="info-card-hero" {
                div class="info-hero-left" {
                    span class="info-hero-title" { (&job.name) }
                    span class=(format!("badge badge-{}", badge_class)) { (&job.status) }
                    span class="badge badge-muted" { "Archived" }
                }
                @if let Some(d) = &duration_str {
                    div class="info-hero-duration" {
                        span class="info-hero-duration-value" { (d) }
                    }
                }
            }
            div class="info-card-grid" {
                div class="info-tile" {
                    span class="info-tile-label" { "Job Template" }
                    span class="info-tile-value" {
                        a href=(format!("/job-templates/{}/{}", job.job_template_namespace, job.job_template_name)) {
                            (&job.job_template_name)
                        }
                    }
                }
                div class="info-tile" {
                    span class="info-tile-label" { "Started" }
                    span class="info-tile-value" {
                        @if let Some(t) = &job.start_time {
                            (t)
                        } @else {
                            span class="text-muted" { "Unknown" }
                        }
                    }
                }
                @if let Some(t) = &job.completion_time {
                    div class="info-tile" {
                        span class="info-tile-label" { "Completed" }
                        span class="info-tile-value" { (t) }
                    }
                }
                div class="info-tile" {
                    span class="info-tile-label" { "Archived At" }
                    span class="info-tile-value" { (&job.archived_at) }
                }
            }
        }

        @let snap_ctx = job.snapshot_diff_json.as_ref()
            .and_then(|diff_json| serde_json::from_str::<SnapshotDiff>(diff_json).ok())
            .map(|diff| {
                let conn = pool.get().ok();
                let file_views = job.output_test_snapshots.as_ref()
                    .and_then(|tarball| snapshot::extract_tarball(tarball).ok())
                    .map(|current| {
                        // Use the job's fixed baseline, not the latest
                        let baseline = match (job.snapshot_baseline_id, &conn) {
                            (Some(id), Some(c)) => crate::db::snapshot::load_baseline_files(id, c),
                            _ => BTreeMap::new(),
                        };
                        build_snapshot_file_views(&diff, &baseline, &current)
                    })
                    .unwrap_or_default();
                let is_latest_run = conn.as_ref()
                    .map(|c| crate::db::snapshot::is_latest_snapshot_job(
                        &job.job_template_name, &job.job_template_namespace,
                        &job.name, &job.namespace, c,
                    ))
                    .unwrap_or(false);
                SnapshotContext {
                    file_views,
                    snapshot_status: job.snapshot_status.clone(),
                    jt_name: job.job_template_name.clone(),
                    jt_namespace: job.job_template_namespace.clone(),
                    baseline_id: job.snapshot_baseline_id,
                    is_latest_run,
                }
            });

        (render_output_sections(&output, &job.namespace, &job.name, snap_ctx.as_ref()))

        (render_events_section(&archived_events))

        h3 class="section-title" { "Logs" }
        @if let Some(log_text) = &job.logs {
            pre class="log-viewer" { (log_text) }
        } @else {
            div class="empty-state-box" { "No logs were captured." }
        }
    }
}

fn render_output_sections(
    output: &JobOutput,
    namespace: &str,
    job_name: &str,
    snap_ctx: Option<&SnapshotContext>,
) -> Markup {
    let has_tests = output.test_results_xml.is_some() || snap_ctx.is_some();
    let has_other = output.result_json.is_some() || output.report_md.is_some() || output.archive.is_some();

    if !has_tests && !has_other {
        return html! {};
    }

    let junit_suites = output.test_results_xml.as_deref()
        .and_then(super::junit::parse_junit_xml);

    html! {
        // Combined test results section (JUnit + snapshots)
        @if has_tests {
            h3 class="section-title" { "Test Results" }

            // Compute combined summary
            @let junit_total = junit_suites.as_ref().map(|s| s.total_tests()).unwrap_or(0);
            @let junit_failed = junit_suites.as_ref().map(|s| s.total_failures() + s.total_errors()).unwrap_or(0);
            @let junit_skipped = junit_suites.as_ref().map(|s| s.total_skipped()).unwrap_or(0);
            @let snap_total = snap_ctx.map(|s| s.file_views.len() as u32).unwrap_or(0);
            @let snap_differing = snap_ctx.map(|s| s.file_views.iter().filter(|f| f.status != FileDiffStatus::Matched).count() as u32).unwrap_or(0);
            @let snap_accepted = snap_ctx.map(|s| s.snapshot_status.as_deref() == Some("accepted_as_baseline")).unwrap_or(false);
            // Accepted diffs don't count as failures
            @let total = junit_total + snap_total;
            @let failed = junit_failed + if !snap_accepted { snap_differing } else { 0 };
            @let all_passed = failed == 0;
            @let can_accept = snap_ctx.map(|s| snap_differing > 0 && !snap_accepted && s.is_latest_run).unwrap_or(false);

            div id="snapshot-section" {
                div class=(if all_passed { "test-summary-bar test-summary-pass" } else { "test-summary-bar test-summary-fail" }) {
                    @if all_passed {
                        span class="test-summary-icon" { "\u{2713}" }
                        " All " (total) " tests passed"
                    } @else {
                        span class="test-summary-icon" { "\u{2717}" }
                        " " (failed) " of " (total) " tests failed"
                    }
                    @if junit_skipped > 0 {
                        span class="test-summary-skipped" { " (" (junit_skipped) " skipped)" }
                    }
                    // Accept button: only for latest run comparing against current baseline
                    @if let Some(ctx) = snap_ctx {
                        @if can_accept {
                            span class="test-summary-skipped" {
                                " "
                                button class="btn btn-sm snapshot-accept-btn"
                                    hx-post=(format!("/api/jobs/{}/{}/snapshots/accept", namespace, job_name))
                                    hx-vals=(format!(r#"{{"job_template_name":"{}","job_template_namespace":"{}"}}"#, ctx.jt_name, ctx.jt_namespace))
                                    hx-target="#snapshot-suite"
                                    hx-swap="outerHTML"
                                    hx-confirm="Accept these snapshots as the new baseline?" {
                                    "Accept as new baseline"
                                }
                            }
                        }
                    }
                }

                // JUnit suites
                @if let Some(ref suites) = junit_suites {
                    @for suite in &suites.suites {
                        (super::junit::render_test_suite(suite))
                    }
                }

                // Snapshot suite
                @if let Some(ctx) = snap_ctx {
                    div id="snapshot-suite" class="test-suite" {
                        div class="test-suite-header" {
                            span class="test-suite-name" { "snapshots" }
                            span class="test-suite-stats" { (snap_total) " files" }
                        }
                        @for file in &ctx.file_views {
                            @let is_diff = file.status != FileDiffStatus::Matched;
                            @let (icon, row_class) = if !is_diff {
                                ("\u{2713}", "test-case test-case-passed")
                            } else if snap_accepted {
                                ("\u{0394}", "test-case test-case-info")
                            } else {
                                ("\u{2717}", "test-case test-case-failed")
                            };
                            @let status_label = match file.status {
                                FileDiffStatus::Added => " (added)",
                                FileDiffStatus::Removed => " (removed)",
                                FileDiffStatus::Changed => " (changed)",
                                FileDiffStatus::Matched => "",
                            };
                            @let baseline_link = match ctx.baseline_id {
                                Some(id) => format!("/snapshots/baseline/{}/{}", id, file.path),
                                None => format!(
                                    "/job-templates/{}/{}/snapshots/baseline/{}",
                                    ctx.jt_namespace, ctx.jt_name, file.path
                                ),
                            };
                            div class=(row_class) {
                                @if is_diff && file.diff_lines.is_some() {
                                    details class="snapshot-details" {
                                        summary class="test-case-header" {
                                            span class="test-case-icon" { (icon) }
                                            span class="test-case-name" { (&file.path) (status_label) }
                                            span class="snapshot-file-links" {
                                                @if file.status != FileDiffStatus::Added {
                                                    a href=(&baseline_link) target="_blank"
                                                        class="btn btn-secondary btn-sm"
                                                        onclick="event.stopPropagation()" {
                                                        "Baseline"
                                                    }
                                                }
                                                a href=(format!(
                                                    "/jobs/{}/{}/snapshots/current/{}",
                                                    namespace, job_name, file.path
                                                )) target="_blank" class="btn btn-secondary btn-sm"
                                                    onclick="event.stopPropagation()" {
                                                    "Current"
                                                }
                                            }
                                        }
                                        div class="test-case-detail" {
                                            div class="snapshot-diff" {
                                                @for line in file.diff_lines.as_ref().unwrap() {
                                                    @match line {
                                                        DiffLine::Context(text) => {
                                                            div class="diff-line diff-context" {
                                                                span class="diff-marker" { " " }
                                                                (text)
                                                            }
                                                        },
                                                        DiffLine::Added(text) => {
                                                            div class="diff-line diff-added" {
                                                                span class="diff-marker" { "+" }
                                                                (text)
                                                            }
                                                        },
                                                        DiffLine::Removed(text) => {
                                                            div class="diff-line diff-removed" {
                                                                span class="diff-marker" { "-" }
                                                                (text)
                                                            }
                                                        },
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } @else if is_diff {
                                    div class="test-case-header" {
                                        span class="test-case-icon" { (icon) }
                                        span class="test-case-name" { (&file.path) (status_label) }
                                        span class="snapshot-file-links" {
                                            @if file.status != FileDiffStatus::Added {
                                                a href=(&baseline_link) target="_blank"
                                                    class="btn btn-secondary btn-sm" {
                                                    "Baseline"
                                                }
                                            }
                                            @if file.status != FileDiffStatus::Removed {
                                                a href=(format!(
                                                    "/jobs/{}/{}/snapshots/current/{}",
                                                    namespace, job_name, file.path
                                                )) target="_blank" class="btn btn-secondary btn-sm" {
                                                    "Current"
                                                }
                                            }
                                        }
                                    }
                                } @else {
                                    div class="test-case-header" {
                                        span class="test-case-icon" { (icon) }
                                        span class="test-case-name" { (&file.path) }
                                        span class="snapshot-file-links" {
                                            a href=(format!(
                                                "/jobs/{}/{}/snapshots/current/{}",
                                                namespace, job_name, file.path
                                            )) target="_blank" class="btn btn-secondary btn-sm" {
                                                "Current"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Non-test output sections
        @if has_other {
            h3 class="section-title" { "Job Output" }

            @if let Some(json) = &output.result_json {
                div class="output-section" {
                    div class="output-section-header" { "result.json" }
                    pre class="output-viewer output-json" { (json) }
                }
            }

            @if let Some(md) = &output.report_md {
                div class="output-section" {
                    div class="output-section-header" { "report.md" }
                    div class="output-viewer output-markdown" {
                        pre { (md) }
                    }
                }
            }

            @if output.archive.is_some() {
                div class="output-section" {
                    div class="output-section-header" { "archive.tar.gz" }
                    div class="output-viewer output-archive" {
                        a href=(format!("/jobs/{}/{}/output/archive.tar.gz", namespace, job_name))
                            class="btn btn-secondary" download {
                            "Download archive.tar.gz"
                        }
                    }
                }
            }
        }

        // Unparseable XML fallback
        @if output.test_results_xml.is_some() && junit_suites.is_none() {
            h3 class="section-title" { "Test Results" }
            div class="output-section" {
                div class="output-section-header" { "test-results.xml" }
                pre class="output-viewer output-xml" { (output.test_results_xml.as_deref().unwrap_or("")) }
            }
        }
    }
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

pub(crate) async fn fetch_live_logs(client: &Client, namespace: &str, job_name: &str) -> Option<String> {
    let pod_api: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client.clone(), namespace);
    let pods = pod_api
        .list(&kube::api::ListParams::default().labels(&format!("job-name={}", job_name)))
        .await
        .ok()?;

    let mut all_logs = Vec::new();
    for pod in &pods.items {
        let pod_name = pod.name_any();
        if let Ok(log_str) = pod_api.logs(&pod_name, &LogParams {
            timestamps: true,
            ..Default::default()
        }).await {
            if pods.items.len() > 1 {
                all_logs.push(format!("=== Pod: {} ===\n{}", pod_name, log_str));
            } else {
                all_logs.push(log_str);
            }
        }
    }

    if all_logs.is_empty() { None } else { Some(all_logs.join("\n")) }
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

    // Only show "Running" once user code is actually running (container started).
    // Before that (scheduling, image pull, init containers), show "Pending".
    if status.active.is_some_and(|a| a > 0) {
        if status.ready.is_some_and(|r| r > 0) {
            return ("info", "Running");
        }
        // Pod exists but container isn't ready yet — still starting up
        return ("muted", "Pending");
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

fn render_events_section(events: &[EventInfo]) -> Markup {
    html! {
        h3 class="section-title" { "Events" }
        @if events.is_empty() {
            div class="empty-state-box" { "No events." }
        } @else {
            table class="jt-table" {
                thead {
                    tr {
                        th { "Time" }
                        th { "Type" }
                        th { "Reason" }
                        th { "Source" }
                        th { "Message" }
                        th { "Count" }
                    }
                }
                tbody {
                    @for event in events {
                        @let row_class = match event.type_.as_str() {
                            "Warning" => "event-warning",
                            _ => "",
                        };
                        tr class=(row_class) {
                            td class="time-cell" { (&event.timestamp) }
                            td {
                                @if event.type_ == "Warning" {
                                    span class="badge badge-danger" { (&event.type_) }
                                } @else {
                                    span class="badge badge-muted" { (&event.type_) }
                                }
                            }
                            td { (&event.reason) }
                            td class="text-muted" { (&event.source) }
                            td { (&event.message) }
                            td class="text-muted" {
                                @if let Some(c) = event.count {
                                    (c)
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
