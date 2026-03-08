use actix_web::{get, web, HttpResponse, Responder};
use k8s_openapi::api::batch::v1::Job;
use kube::{Api, Client, ResourceExt};
use maud::{html, DOCTYPE};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

use crate::kubernetes::JobTemplate;
use crate::metrics::is_acceptance_failure;

type Conn = r2d2::PooledConnection<SqliteConnectionManager>;

#[get("/job-templates")]
pub async fn job_templates_page(
    _client: web::Data<Client>,
) -> impl Responder {
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                title { "Job Templates" }
                (crate::web::header::stylesheet_link())
                (crate::web::header::scripts())
            }
            body hx-ext="morph" {
                (crate::web::header::render("job-templates"))
                div class="content" {
                    div class="job-templates-content"
                        hx-get="/job-templates-fragment"
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

struct TemplateRow {
    name: String,
    namespace: String,
    schedule: Option<String>,
    /// (badge_class, status_text)
    last_run: Option<(&'static str, &'static str)>,
    /// None if not AT or no completed runs; Some(true) = passed, Some(false) = failed
    at_status: Option<bool>,
}

#[get("/job-templates-fragment")]
pub async fn job_templates_fragment(
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let api: Api<JobTemplate> = Api::all(client.get_ref().clone());
    let job_templates = match api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::error!("Failed to list JobTemplates: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body(format!("Failed to list JobTemplates: {}", e));
        }
    };

    let job_api: Api<Job> = Api::all(client.get_ref().clone());
    let live_jobs: Vec<Job> = match job_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::warn!("Failed to list Jobs: {}", e);
            vec![]
        }
    };

    let conn = pool.get().ok();

    let rows: Vec<TemplateRow> = job_templates
        .iter()
        .map(|jt| {
            let jt_uid = jt.uid().unwrap_or_default();
            let name = jt.name_any();
            let namespace = jt.namespace().unwrap_or_default();

            let last_run =
                most_recent_status(&jt_uid, &live_jobs, &name, &namespace, conn.as_ref());
            let at_status = if jt.is_acceptance_test() {
                at_status_for_template(&jt_uid, &live_jobs, &name, &namespace, conn.as_ref())
            } else {
                None
            };

            TemplateRow {
                name,
                namespace,
                schedule: jt.spec.schedule.clone(),
                last_run,
                at_status,
            }
        })
        .collect();

    let markup = html! {
        table class="jt-table" {
            thead {
                tr {
                    th { "Name" }
                    th { "Namespace" }
                    th { "Schedule" }
                    th { "Last Run" }
                    th { "AT Status" }
                }
            }
            tbody {
                @if rows.is_empty() {
                    tr {
                        td colspan="5" class="empty-state" {
                            "No JobTemplates found in the cluster."
                        }
                    }
                }
                @for row in &rows {
                    tr {
                        td class="jt-name" {
                            a href=(format!("/job-templates/{}/{}", row.namespace, row.name)) {
                                (row.name)
                            }
                        }
                        td { (row.namespace) }
                        td class="jt-schedule" {
                            @if let Some(schedule) = &row.schedule {
                                code { (schedule) }
                            } @else {
                                span class="text-muted" { "On-demand" }
                            }
                        }
                        td {
                            @if let Some((badge_class, status_text)) = row.last_run {
                                span class=(format!("badge badge-{badge_class}")) { (status_text) }
                            } @else {
                                span class="text-muted" { "-" }
                            }
                        }
                        td {
                            @if let Some(passed) = row.at_status {
                                @if passed {
                                    span class="badge badge-success" { "Passed" }
                                } @else {
                                    span class="badge badge-danger" { "Failed" }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

/// Get the status of the most recent job (live or archived) for a template.
fn most_recent_status(
    jt_uid: &str,
    live_jobs: &[Job],
    jt_name: &str,
    jt_namespace: &str,
    conn: Option<&Conn>,
) -> Option<(&'static str, &'static str)> {
    // Find the most recent live job owned by this template
    let mut best_live: Option<(&Job, String)> = None;
    for job in live_jobs {
        let is_owner = job
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|refs| refs.iter().any(|r| r.uid == jt_uid));
        if !is_owner {
            continue;
        }
        let start = job
            .status
            .as_ref()
            .and_then(|s| s.start_time.as_ref())
            .map(|t| t.0.to_rfc3339())
            .or_else(|| {
                job.metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|t| t.0.to_rfc3339())
            })
            .unwrap_or_default();
        if best_live.as_ref().is_none_or(|(_, t)| start > *t) {
            best_live = Some((job, start));
        }
    }

    // Find the most recent archived job
    let archived: Option<(String, String)> = conn.and_then(|c| {
        c.query_row(
            "SELECT status, start_time FROM archived_job
             WHERE job_template_name = ?1 AND job_template_namespace = ?2
             ORDER BY start_time DESC LIMIT 1",
            params![jt_name, jt_namespace],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok()
    });

    match (&best_live, &archived) {
        (Some((job, live_start)), Some((_, arch_start))) => {
            if live_start >= arch_start {
                Some(live_job_status(job))
            } else {
                Some(archived_status_badge(&archived.unwrap().0))
            }
        }
        (Some((job, _)), None) => Some(live_job_status(job)),
        (None, Some((status, _))) => Some(archived_status_badge(status)),
        (None, None) => None,
    }
}

fn live_job_status(job: &Job) -> (&'static str, &'static str) {
    let Some(status) = job.status.as_ref() else {
        return ("muted", "Pending");
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
        if status.ready.is_some_and(|r| r > 0) {
            return ("info", "Running");
        }
        return ("muted", "Pending");
    }

    ("muted", "Pending")
}

fn archived_status_badge(status: &str) -> (&'static str, &'static str) {
    match status {
        "Succeeded" => ("success", "Succeeded"),
        "Failed" => ("danger", "Failed"),
        _ => ("muted", "Unknown"),
    }
}

/// Determine AT pass/fail for the most recent completed run of an acceptance test template.
fn at_status_for_template(
    jt_uid: &str,
    live_jobs: &[Job],
    jt_name: &str,
    jt_namespace: &str,
    conn: Option<&Conn>,
) -> Option<bool> {
    // Find the most recent completed live job
    let mut best_live: Option<(String, Option<String>, Option<String>, String)> = None;
    for job in live_jobs {
        let is_owner = job
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|refs| refs.iter().any(|r| r.uid == jt_uid));
        if !is_owner {
            continue;
        }

        let Some(status) = job.status.as_ref() else {
            continue;
        };
        let status_str = status.conditions.as_ref().and_then(|conds| {
            for c in conds {
                if c.type_ == "Complete" && c.status == "True" {
                    return Some("Succeeded".to_string());
                }
                if c.type_ == "Failed" && c.status == "True" {
                    return Some("Failed".to_string());
                }
            }
            None
        });
        let Some(status_str) = status_str else {
            continue;
        };

        let start_time = status
            .start_time
            .as_ref()
            .map(|t| t.0.to_rfc3339())
            .unwrap_or_default();

        if best_live.as_ref().is_none_or(|(_, _, _, t)| start_time > *t) {
            let job_name = job.metadata.name.as_deref().unwrap_or("");
            let job_ns = job.metadata.namespace.as_deref().unwrap_or("default");
            let junit_xml = conn.and_then(|c| {
                crate::db::job_output::get_string(job_name, job_ns, "test-results.xml", c)
            });
            let snapshot_status = conn.and_then(|c| {
                crate::db::job_output::get_string(job_name, job_ns, "_snapshot_status", c)
            });
            best_live = Some((status_str, junit_xml, snapshot_status, start_time));
        }
    }

    // Find the most recent archived completed job
    let archived: Option<(String, Option<String>, Option<String>, String)> = conn.and_then(|c| {
        c.query_row(
            "SELECT status, output_test_results_xml, snapshot_status, start_time
             FROM archived_job
             WHERE job_template_name = ?1 AND job_template_namespace = ?2
             ORDER BY start_time DESC LIMIT 1",
            params![jt_name, jt_namespace],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .ok()
    });

    // Pick the most recent completed job
    let (status, junit_xml, snapshot_status) = match (&best_live, &archived) {
        (Some((_, _, _, lt)), Some((_, _, _, at))) => {
            if lt >= at {
                let r = best_live.unwrap();
                (r.0, r.1, r.2)
            } else {
                let r = archived.unwrap();
                (r.0, r.1, r.2)
            }
        }
        (Some(_), None) => {
            let r = best_live.unwrap();
            (r.0, r.1, r.2)
        }
        (None, Some(_)) => {
            let r = archived.unwrap();
            (r.0, r.1, r.2)
        }
        (None, None) => return None,
    };

    Some(!is_acceptance_failure(
        &status,
        junit_xml.as_deref(),
        snapshot_status.as_deref(),
    ))
}
