use actix_web::{get, web, HttpResponse, Responder};
use k8s_openapi::api::batch::v1::Job;
use kube::api::LogParams;
use kube::{Api, Client, ResourceExt};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::ArchivedJob;
use crate::kubernetes::events::{self, EventInfo};
use crate::kubernetes::job_output::JobOutput;

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
                    let markup = render_archived_job(&job);
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
        div class="jt-info-card" {
            div class="jt-info-card-body" {
                div class="jt-info-row" {
                    span class="jt-info-label" { "Source" }
                    span class="jt-info-value" {
                        span class="badge badge-info" { "Live" }
                    }
                }
                div class="jt-info-row" {
                    span class="jt-info-label" { "Status" }
                    span class="jt-info-value" {
                        span class=(format!("badge badge-{}", status.0)) { (status.1) }
                    }
                }
                @if let Some(jt) = jt_ref {
                    div class="jt-info-row" {
                        span class="jt-info-label" { "Job Template" }
                        span class="jt-info-value" {
                            a href=(format!("/job-templates/{}/{}", namespace, jt.name)) { (&jt.name) }
                        }
                    }
                }
                div class="jt-info-row" {
                    span class="jt-info-label" { "Started" }
                    span class="jt-info-value" {
                        @if let Some(t) = &start_time {
                            code { (t) }
                        } @else {
                            span class="text-muted" { "Not started" }
                        }
                    }
                }
                @if let Some(t) = &completion_time {
                    div class="jt-info-row" {
                        span class="jt-info-label" { "Completed" }
                        span class="jt-info-value" {
                            code { (t) }
                        }
                    }
                }
                @if !duration.is_empty() {
                    div class="jt-info-row" {
                        span class="jt-info-label" { "Duration" }
                        span class="jt-info-value" { (duration) }
                    }
                }
            }
        }

        (render_events_section(&events))

        (render_output_sections(&output, namespace, &job_name))

        h3 class="section-title" { "Logs" }
        @if let Some(log_text) = &logs {
            pre class="log-viewer" { (log_text) }
        } @else {
            div class="empty-state-box" { "No logs available." }
        }
    }
}

fn render_archived_job(job: &ArchivedJob) -> Markup {
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
    };

    let archived_events: Vec<EventInfo> = job
        .events_json
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default();

    html! {
        div class="jt-info-card" {
            div class="jt-info-card-body" {
                div class="jt-info-row" {
                    span class="jt-info-label" { "Source" }
                    span class="jt-info-value" {
                        span class="badge badge-muted" { "Archived" }
                    }
                }
                div class="jt-info-row" {
                    span class="jt-info-label" { "Status" }
                    span class="jt-info-value" {
                        span class=(format!("badge badge-{}", badge_class)) { (&job.status) }
                    }
                }
                div class="jt-info-row" {
                    span class="jt-info-label" { "Job Template" }
                    span class="jt-info-value" {
                        a href=(format!("/job-templates/{}/{}", job.job_template_namespace, job.job_template_name)) {
                            (&job.job_template_name)
                        }
                    }
                }
                div class="jt-info-row" {
                    span class="jt-info-label" { "Started" }
                    span class="jt-info-value" {
                        @if let Some(t) = &job.start_time {
                            code { (t) }
                        } @else {
                            span class="text-muted" { "Unknown" }
                        }
                    }
                }
                @if let Some(t) = &job.completion_time {
                    div class="jt-info-row" {
                        span class="jt-info-label" { "Completed" }
                        span class="jt-info-value" {
                            code { (t) }
                        }
                    }
                }
                @if let Some(d) = job.duration_seconds {
                    div class="jt-info-row" {
                        span class="jt-info-label" { "Duration" }
                        span class="jt-info-value" { (format_duration(d)) }
                    }
                }
                div class="jt-info-row" {
                    span class="jt-info-label" { "Archived At" }
                    span class="jt-info-value" {
                        code { (&job.archived_at) }
                    }
                }
            }
        }

        (render_events_section(&archived_events))

        (render_output_sections(&output, &job.namespace, &job.name))

        h3 class="section-title" { "Logs" }
        @if let Some(log_text) = &job.logs {
            pre class="log-viewer" { (log_text) }
        } @else {
            div class="empty-state-box" { "No logs were captured." }
        }
    }
}

fn render_output_sections(output: &JobOutput, namespace: &str, job_name: &str) -> Markup {
    if !output.has_any() {
        return html! {};
    }

    html! {
        h3 class="section-title" { "Job Output" }

        // Test results first — they're the most important output type
        @if let Some(xml) = &output.test_results_xml {
            div class="output-section" {
                @if let Some(suites) = super::junit::parse_junit_xml(xml) {
                    (super::junit::render_test_results(&suites))
                } @else {
                    div class="output-section-header" { "test-results.xml" }
                    pre class="output-viewer output-xml" { (xml) }
                }
            }
        }

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

async fn fetch_live_logs(client: &Client, namespace: &str, job_name: &str) -> Option<String> {
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
