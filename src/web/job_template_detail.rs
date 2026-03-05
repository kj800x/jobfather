use actix_web::{get, web, HttpResponse, Responder};
use k8s_openapi::api::batch::v1::Job;
use kube::{Api, Client, ResourceExt};
use maud::{html, Markup, DOCTYPE};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::ArchivedJob;
use crate::kubernetes::JobTemplate;

#[get("/job-templates/{namespace}/{name}")]
pub async fn job_template_detail_page(
    path: web::Path<(String, String)>,
) -> impl Responder {
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
                        hx-get=(format!("/job-templates/{}/{}/fragment", namespace, name))
                        hx-trigger="load, every 5s"
                        hx-swap="morph:innerHTML" {
                        div { "Loading..." }
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

#[get("/job-templates/{namespace}/{name}/fragment")]
pub async fn job_template_detail_fragment(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    // Fetch the JobTemplate itself
    let jt_api: Api<JobTemplate> = Api::namespaced(client.get_ref().clone(), &namespace);
    let job_template = match jt_api.get(&name).await {
        Ok(jt) => jt,
        Err(e) => {
            log::error!("Failed to get JobTemplate {}/{}: {}", namespace, name, e);
            return HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
                .body(format!("JobTemplate {}/{} not found: {}", namespace, name, e));
        }
    };

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
        Ok(conn) => ArchivedJob::get_by_job_template(&name, &namespace, &conn).unwrap_or_else(|e| {
            log::error!("Failed to query archived jobs: {}", e);
            vec![]
        }),
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            vec![]
        }
    };

    let markup = html! {
        // JobTemplate info card
        div class="jt-info-card" {
            div class="jt-info-row" {
                span class="jt-info-label" { "Schedule" }
                span class="jt-info-value" {
                    @if let Some(schedule) = &job_template.spec.schedule {
                        code { (schedule) }
                    } @else {
                        span class="text-muted" { "On-demand" }
                    }
                }
            }
            div class="jt-info-row" {
                span class="jt-info-label" { "Acceptance Test" }
                span class="jt-info-value" {
                    @if job_template.is_acceptance_test() {
                        span class="badge badge-info" { "Yes" }
                    } @else {
                        span class="text-muted" { "No" }
                    }
                }
            }
        }

        // Live Jobs section
        h3 class="section-title" { "Active Jobs" }
        (render_live_jobs_table(&live_jobs))

        // Archived Jobs section
        h3 class="section-title" { "Job History" }
        (render_archived_jobs_table(&archived_jobs))
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

fn render_live_jobs_table(jobs: &[Job]) -> Markup {
    html! {
        table class="jt-table" {
            thead {
                tr {
                    th { "Name" }
                    th { "Status" }
                    th { "Started" }
                    th { "Duration" }
                }
            }
            tbody {
                @if jobs.is_empty() {
                    tr {
                        td colspan="4" class="empty-state" {
                            "No active jobs."
                        }
                    }
                }
                @for job in jobs {
                    @let status = job_status(job);
                    tr {
                        td class="jt-name" { (job.name_any()) }
                        td {
                            span class=(format!("badge badge-{}", status.0)) { (status.1) }
                        }
                        td class="time-cell" {
                            @if let Some(start) = job.status.as_ref().and_then(|s| s.start_time.as_ref()) {
                                (start.0.format("%Y-%m-%d %H:%M:%S"))
                            }
                        }
                        td class="time-cell" {
                            (job_duration_display(job))
                        }
                    }
                }
            }
        }
    }
}

fn render_archived_jobs_table(jobs: &[ArchivedJob]) -> Markup {
    html! {
        table class="jt-table" {
            thead {
                tr {
                    th { "Name" }
                    th { "Status" }
                    th { "Started" }
                    th { "Completed" }
                    th { "Duration" }
                }
            }
            tbody {
                @if jobs.is_empty() {
                    tr {
                        td colspan="5" class="empty-state" {
                            "No archived jobs yet."
                        }
                    }
                }
                @for job in jobs {
                    tr {
                        td class="jt-name" { (job.name) }
                        td {
                            @let badge_class = match job.status.as_str() {
                                "Succeeded" => "success",
                                "Failed" => "danger",
                                "Running" => "info",
                                _ => "muted",
                            };
                            span class=(format!("badge badge-{}", badge_class)) { (job.status) }
                        }
                        td class="time-cell" {
                            @if let Some(t) = &job.start_time {
                                (t)
                            }
                        }
                        td class="time-cell" {
                            @if let Some(t) = &job.completion_time {
                                (t)
                            }
                        }
                        td class="time-cell" {
                            @if let Some(d) = job.duration_seconds {
                                (format_duration(d))
                            }
                        }
                    }
                }
            }
        }
    }
}

fn job_status(job: &Job) -> (&str, &str) {
    let status = match job.status.as_ref() {
        Some(s) => s,
        None => return ("muted", "Unknown"),
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

    if status.active.is_some_and(|a| a > 0) {
        return ("info", "Running");
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
