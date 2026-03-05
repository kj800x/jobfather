use actix_web::{get, web, HttpResponse, Responder};
use kube::{Api, Client, ResourceExt};
use maud::{html, DOCTYPE};

use crate::kubernetes::JobTemplate;

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

#[get("/job-templates-fragment")]
pub async fn job_templates_fragment(
    client: web::Data<Client>,
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

    let markup = html! {
        table class="jt-table" {
            thead {
                tr {
                    th { "Name" }
                    th { "Namespace" }
                    th { "Schedule" }
                    th { "Acceptance Test" }
                }
            }
            tbody {
                @if job_templates.is_empty() {
                    tr {
                        td colspan="4" class="empty-state" {
                            "No JobTemplates found in the cluster."
                        }
                    }
                }
                @for jt in &job_templates {
                    tr {
                        td class="jt-name" {
                            a href=(format!("/job-templates/{}/{}", jt.namespace().unwrap_or_default(), jt.name_any())) {
                                (jt.name_any())
                            }
                        }
                        td { (jt.namespace().unwrap_or_default()) }
                        td class="jt-schedule" {
                            @if let Some(schedule) = &jt.spec.schedule {
                                code { (schedule) }
                            } @else {
                                span class="text-muted" { "On-demand" }
                            }
                        }
                        td {
                            @if jt.is_acceptance_test() {
                                span class="badge badge-info" { "Yes" }
                            } @else {
                                span class="text-muted" { "No" }
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
