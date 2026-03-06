use actix_web::{get, post, web, HttpResponse, Responder};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::PostParams;
use kube::{Api, Client, ResourceExt};
use maud::{html, Markup, DOCTYPE};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::Deserialize;

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
                    div id="modal-container" {}
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
        Ok(list) => {
            let mut jobs: Vec<Job> = list
                .items
                .into_iter()
                .filter(|job| {
                    job.metadata
                        .owner_references
                        .as_ref()
                        .is_some_and(|refs| refs.iter().any(|r| r.uid == jt_uid))
                })
                .collect();
            jobs.sort_by(|a, b| {
                let a_time = a.status.as_ref().and_then(|s| s.start_time.as_ref());
                let b_time = b.status.as_ref().and_then(|s| s.start_time.as_ref());
                b_time.cmp(&a_time)
            });
            jobs
        }
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
            div class="jt-info-card-body" {
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
                    span class="jt-info-label" { "Cleanup After" }
                    span class="jt-info-value" {
                        code { (job_template.cleanup_after()) }
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
            div class="jt-info-actions" {
                button class="btn btn-primary"
                    hx-get=(format!("/job-templates/{}/{}/run-modal", namespace, name))
                    hx-target="#modal-container"
                    hx-swap="innerHTML" {
                    "Run Job"
                }
            }
        }

        h3 class="section-title" { "Live Jobs" }
        (render_live_jobs_table(&live_jobs, &namespace))

        h3 class="section-title" { "Archived Jobs" }
        (render_archived_jobs_table(&archived_jobs))
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

fn render_live_jobs_table(jobs: &[Job], namespace: &str) -> Markup {
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
                            "No live jobs."
                        }
                    }
                }
                @for job in jobs {
                    @let status = job_status(job);
                    tr {
                        td class="jt-name" {
                            a href=(format!("/jobs/{}/{}", namespace, job.name_any())) { (job.name_any()) }
                        }
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
                        td class="jt-name" {
                            a href=(format!("/jobs/{}/{}", job.namespace, job.name)) { (&job.name) }
                        }
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

    // Extract current args and env from the first container in the pod spec
    let pod_spec = &job_template.spec.spec;
    let first_container = pod_spec
        .get("containers")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first());

    let current_args: Vec<String> = first_container
        .and_then(|c| c.get("args"))
        .and_then(|a| a.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|a| a.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let current_command: Vec<String> = first_container
        .and_then(|c| c.get("command"))
        .and_then(|a| a.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|a| a.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Separate env vars into editable (plain value) and non-editable (valueFrom / secretKeyRef etc.)
    let env_array = first_container
        .and_then(|c| c.get("env"))
        .and_then(|e| e.as_array());

    let mut editable_env: Vec<(String, String)> = Vec::new();
    let mut ref_env: Vec<(String, String)> = Vec::new(); // (name, source description)

    if let Some(envs) = env_array {
        for e in envs {
            let Some(env_name) = e.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if e.get("valueFrom").is_some() {
                // Describe the source for display
                let source = describe_value_from(e.get("valueFrom").unwrap());
                ref_env.push((env_name.to_string(), source));
            } else {
                let value = e.get("value").and_then(|v| v.as_str()).unwrap_or("");
                editable_env.push((env_name.to_string(), value.to_string()));
            }
        }
    }

    let container_image = first_container
        .and_then(|c| c.get("image"))
        .and_then(|i| i.as_str())
        .unwrap_or("unknown");

    let args_text = current_args.join("\n");

    let markup = html! {
        dialog id="run-job-dialog" class="modal" open {
            div class="modal-backdrop" onclick="this.closest('dialog').remove()" {}
            div class="modal-content" {
                div class="modal-header" {
                    h3 { "Run Job: " (name) }
                    button class="modal-close" onclick="this.closest('dialog').remove()" { "x" }
                }
                form
                    hx-post=(format!("/job-templates/{}/{}/run", namespace, name))
                    hx-target="#modal-container"
                    hx-swap="innerHTML" {
                    div class="modal-body" {
                        div class="form-info" {
                            span class="form-info-label" { "Image" }
                            code { (container_image) }
                        }
                        @if !current_command.is_empty() {
                            div class="form-info" {
                                span class="form-info-label" { "Command" }
                                code { (current_command.join(" ")) }
                            }
                        }
                        div class="form-group" {
                            label for="args" { "Arguments " span class="text-muted" { "(one per line)" } }
                            textarea id="args" name="args" class="form-control" rows="4" {
                                (args_text)
                            }
                        }
                        @if !ref_env.is_empty() {
                            div class="form-group" {
                                label { "Environment Variables " span class="text-muted" { "(from secrets/refs - inherited automatically)" } }
                                @for (env_name, source) in &ref_env {
                                    div class="env-row env-row-readonly" {
                                        span class="env-ref-name" { (env_name) }
                                        span class="env-ref-source" { (source) }
                                    }
                                }
                            }
                        }
                        div class="form-group" {
                            label { "Environment Variables " span class="text-muted" { "(editable)" } }
                            @for (i, (env_name, env_value)) in editable_env.iter().enumerate() {
                                div class="env-row" {
                                    input type="text" name=(format!("env_name_{}", i)) value=(env_name)
                                        class="form-control env-name" placeholder="NAME";
                                    input type="text" name=(format!("env_value_{}", i)) value=(env_value)
                                        class="form-control env-value" placeholder="value";
                                }
                            }
                            // Extra empty rows for adding new env vars
                            @for i in editable_env.len()..(editable_env.len() + 3) {
                                div class="env-row" {
                                    input type="text" name=(format!("env_name_{}", i))
                                        class="form-control env-name" placeholder="NAME";
                                    input type="text" name=(format!("env_value_{}", i))
                                        class="form-control env-value" placeholder="value";
                                }
                            }
                        }
                    }
                    div class="modal-footer" {
                        button type="button" class="btn btn-secondary" onclick="this.closest('dialog').remove()" { "Cancel" }
                        button type="submit" class="btn btn-primary" { "Run" }
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

#[derive(Deserialize)]
pub struct RunJobForm {
    args: Option<String>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, String>,
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

    // Parse args from the textarea (one per line, skip blanks)
    let args: Vec<String> = form
        .args
        .as_deref()
        .unwrap_or("")
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // Parse editable env vars from the form's flat key-value pairs
    let mut form_env: Vec<(String, String)> = Vec::new();
    for i in 0..100 {
        let env_name_key = format!("env_name_{}", i);
        let env_value_key = format!("env_value_{}", i);
        match (form.extra.get(&env_name_key), form.extra.get(&env_value_key)) {
            (Some(env_name), Some(env_value)) if !env_name.is_empty() => {
                form_env.push((env_name.clone(), env_value.clone()));
            }
            _ => {
                if i > 20 { break; }
            }
        }
    }

    // Build the Job from the template's pod spec
    let mut pod_spec: serde_json::Value = job_template.spec.spec.clone();

    // Inject emptyDir volume for job output
    if pod_spec.get("volumes").is_none() {
        pod_spec["volumes"] = serde_json::json!([]);
    }
    if let Some(volumes) = pod_spec["volumes"].as_array_mut() {
        volumes.push(serde_json::json!({"name": "job-output", "emptyDir": {}}));
    }

    // Override args and env on the first container, and add volume mount
    if let Some(containers) = pod_spec.get_mut("containers").and_then(|c| c.as_array_mut()) {
        if let Some(container) = containers.first_mut() {
            // Add /job-output volume mount
            if container.get("volumeMounts").is_none() {
                container["volumeMounts"] = serde_json::json!([]);
            }
            if let Some(mounts) = container["volumeMounts"].as_array_mut() {
                mounts.push(serde_json::json!({"name": "job-output", "mountPath": "/job-output"}));
            }
            if !args.is_empty() || job_template.spec.spec.get("containers")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("args"))
                .is_some()
            {
                container["args"] = serde_json::to_value(&args).unwrap_or_default();
            }

            // Rebuild env: keep all valueFrom entries from the template, replace plain value entries
            let original_env = container
                .get("env")
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default();

            let mut new_env: Vec<serde_json::Value> = Vec::new();

            // Preserve all valueFrom entries as-is
            for entry in &original_env {
                if entry.get("valueFrom").is_some() {
                    new_env.push(entry.clone());
                }
            }

            // Add the editable env vars from the form
            for (name, value) in &form_env {
                new_env.push(serde_json::json!({
                    "name": name,
                    "value": value,
                }));
            }

            container["env"] = serde_json::to_value(&new_env).unwrap_or_default();
        }
    }

    // Inject job-output sidecar container
    let jobfather_url = std::env::var("JOBFATHER_URL")
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    if !jobfather_url.is_empty() {
        if pod_spec.get("initContainers").is_none() {
            pod_spec["initContainers"] = serde_json::json!([]);
        }
        if let Some(init_containers) = pod_spec["initContainers"].as_array_mut() {
            init_containers.push(serde_json::json!({
                "name": "job-output-sidecar",
                "image": "curlimages/curl:latest",
                "restartPolicy": "Always",
                "command": ["sh", "-c", concat!(
                    "upload() { ",
                    "for f in result.json report.md test-results.xml archive.tar.gz; do ",
                    "[ -f \"/job-output/$f\" ] && curl -sf -X PUT --data-binary \"@/job-output/$f\" ",
                    "\"$JOBFATHER_URL/api/jobs/$JOBFATHER_NAMESPACE/$JOBFATHER_JOB_NAME/output/$f\" || true; ",
                    "done; exit 0; }; ",
                    "trap upload TERM; ",
                    "while true; do sleep 3600 & wait; done"
                )],
                "env": [
                    {"name": "JOBFATHER_URL", "value": &jobfather_url},
                    {"name": "JOBFATHER_NAMESPACE", "value": &namespace},
                    {"name": "JOBFATHER_JOB_NAME", "valueFrom": {"fieldRef": {"fieldPath": "metadata.labels['job-name']"}}}
                ],
                "volumeMounts": [{"name": "job-output", "mountPath": "/job-output"}]
            }));
        }
    }

    // Add restartPolicy if not set
    if pod_spec.get("restartPolicy").is_none() {
        pod_spec["restartPolicy"] = serde_json::Value::String("Never".to_string());
    }

    let pod_template: k8s_openapi::api::core::v1::PodSpec =
        match serde_json::from_value(pod_spec) {
            Ok(spec) => spec,
            Err(e) => {
                log::error!("Failed to deserialize pod spec: {}", e);
                return HttpResponse::InternalServerError()
                    .content_type("text/html; charset=utf-8")
                    .body(render_modal_result(false, &format!("Invalid pod spec: {}", e)));
            }
        };

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let job_name = format!("{}-{}", name, timestamp);

    let job = Job {
        metadata: ObjectMeta {
            name: Some(job_name.clone()),
            namespace: Some(namespace.clone()),
            owner_references: Some(vec![
                k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                    api_version: "jobfather.coolkev.com/v1".to_string(),
                    kind: "JobTemplate".to_string(),
                    name: name.clone(),
                    uid: job_template.uid().unwrap_or_default(),
                    controller: Some(true),
                    block_owner_deletion: Some(true),
                },
            ]),
            ..Default::default()
        },
        spec: Some(JobSpec {
            template: PodTemplateSpec {
                spec: Some(pod_template),
                ..Default::default()
            },
            backoff_limit: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    };

    let job_api: Api<Job> = Api::namespaced(client.get_ref().clone(), &namespace);
    match job_api.create(&PostParams::default(), &job).await {
        Ok(_) => {
            log::info!("Created job {} in namespace {}", job_name, namespace);
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(render_modal_result(true, &format!("Job {} created", job_name)))
        }
        Err(e) => {
            log::error!("Failed to create job: {}", e);
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(render_modal_result(false, &format!("Failed to create job: {}", e)))
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

    if status.active.is_some_and(|a| a > 0) {
        if status.ready.is_some_and(|r| r > 0) {
            return ("info", "Running");
        }
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
