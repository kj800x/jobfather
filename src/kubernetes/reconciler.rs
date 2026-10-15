use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use kube::api::{DeleteParams, LogParams};
use kube::runtime::Controller;
use kube::runtime::controller::Action;
use kube::{Api, Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::{ArchivedJob, archived_job::ArchivedJobEgg};
use crate::kubernetes::JobTemplate;
use crate::metrics::Metrics;

struct Context {
    client: Client,
    pool: Pool<SqliteConnectionManager>,
    metrics: Arc<Metrics>,
}

pub async fn run(client: Client, pool: Pool<SqliteConnectionManager>, metrics: Arc<Metrics>) {
    let job_api: Api<Job> = Api::all(client.clone());

    let ctx = Arc::new(Context {
        client: client.clone(),
        pool,
        metrics,
    });

    Controller::new(job_api, Default::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(obj) => log::trace!("Reconciled {:?}", obj),
                Err(e) => log::warn!("Reconcile error: {:?}", e),
            }
        })
        .await;
}

async fn reconcile(job: Arc<Job>, ctx: Arc<Context>) -> Result<Action, kube::Error> {
    // Only process jobs owned by a JobTemplate
    let owner_ref = match job_template_owner(&job) {
        Some(r) => r,
        None => return Ok(Action::await_change()),
    };

    let namespace = job.namespace().unwrap_or_default();
    let job_name = job.name_any();

    // Determine if job is in a terminal state and when it finished
    let completion_time = match terminal_time(&job) {
        Some(t) => t,
        None => {
            // Job still running — check back in a bit
            return Ok(Action::requeue(Duration::from_secs(30)));
        }
    };

    record_completion(&job, &ctx.metrics);

    // Fetch the parent JobTemplate to get cleanupAfter
    let jt_api: Api<JobTemplate> = Api::namespaced(ctx.client.clone(), &namespace);
    let job_template = match jt_api.get(&owner_ref.name).await {
        Ok(jt) => jt,
        Err(e) => {
            log::warn!(
                "Failed to get JobTemplate {} for job {}/{}: {}",
                owner_ref.name,
                namespace,
                job_name,
                e
            );
            // If the parent is gone, clean up immediately
            if matches!(e, kube::Error::Api(ref resp) if resp.code == 404) {
                return Ok(Action::requeue(Duration::from_secs(60)));
            }
            return Ok(Action::requeue(Duration::from_secs(30)));
        }
    };

    let cleanup_duration = parse_duration(job_template.cleanup_after());
    let elapsed = Utc::now() - completion_time;

    if elapsed < cleanup_duration {
        // Not yet time to clean up — requeue for when it's due
        let remaining = (cleanup_duration - elapsed)
            .to_std()
            .unwrap_or(Duration::from_secs(30));
        return Ok(Action::requeue(remaining));
    }

    match archive_and_delete(&job, &ctx.client, &ctx.pool, &ctx.metrics).await {
        Ok(()) => Ok(Action::await_change()),
        Err(e) => {
            log::error!("{}", e);
            Ok(Action::requeue(Duration::from_secs(30)))
        }
    }
}

fn job_template_owner(
    job: &Job,
) -> Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference> {
    job.metadata
        .owner_references
        .as_ref()
        .and_then(|refs| refs.iter().find(|r| r.kind == "JobTemplate"))
}

/// Record completion metrics for a finished job (once per job UID).
fn record_completion(job: &Job, metrics: &Metrics) {
    let (Some(owner_ref), Some(completion_time)) = (job_template_owner(job), terminal_time(job))
    else {
        return;
    };
    let duration_seconds = job
        .status
        .as_ref()
        .and_then(|s| s.start_time.as_ref())
        .map(|t| (completion_time - t.0).num_seconds());
    metrics.record_job_completion_once(
        &job.uid().unwrap_or_default(),
        &job.namespace().unwrap_or_default(),
        &owner_ref.name,
        &job_status_string(job),
        duration_seconds,
    );
}

/// Archive a finished job (logs, events, output) to SQLite and delete it from
/// Kubernetes. Used by the reconciler once `cleanupAfter` has elapsed, and by the
/// UI's "Clean up" button to do it early.
pub async fn archive_and_delete(
    job: &Job,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    metrics: &Metrics,
) -> Result<(), String> {
    let namespace = job.namespace().unwrap_or_default();
    let job_name = job.name_any();
    let job_uid = job.uid().unwrap_or_default();

    let owner_ref = job_template_owner(job).ok_or_else(|| {
        format!(
            "Job {}/{} is not owned by a JobTemplate",
            namespace, job_name
        )
    })?;
    let completion_time = terminal_time(job)
        .ok_or_else(|| format!("Job {}/{} has not finished yet", namespace, job_name))?;

    // Make sure completion metrics aren't lost if this runs before the reconciler saw the job finish
    record_completion(job, metrics);

    log::info!("Archiving and deleting job {}/{}", namespace, job_name);

    // Fetch logs, events, and job output from the job's pod(s)
    let logs = fetch_job_logs(client, &namespace, &job_name).await;
    let events = crate::kubernetes::events::fetch_job_events(client, &namespace, job).await;
    let output = crate::kubernetes::job_output::JobOutput::load(&job_name, &namespace, pool);

    let status = job_status_string(job);
    let start_time = job
        .status
        .as_ref()
        .and_then(|s| s.start_time.as_ref())
        .map(|t| t.0.to_rfc3339());
    let comp_time_str = Some(completion_time.to_rfc3339());
    let duration_seconds = job
        .status
        .as_ref()
        .and_then(|s| s.start_time.as_ref())
        .map(|t| (completion_time - t.0).num_seconds());

    // Read snapshot status computed at upload time (not recomputed here)
    let (snapshot_status, snapshot_diff_json, snapshot_baseline_id) = match pool.get() {
        Ok(conn) => {
            let status =
                crate::db::job_output::get_string(&job_name, &namespace, "_snapshot_status", &conn);
            let diff = crate::db::job_output::get_string(
                &job_name,
                &namespace,
                "_snapshot_diff_json",
                &conn,
            );
            let baseline_id = crate::db::job_output::get_string(
                &job_name,
                &namespace,
                "_snapshot_baseline_id",
                &conn,
            )
            .and_then(|s| s.parse::<i64>().ok());
            (status, diff, baseline_id)
        }
        Err(_) => (None, None, None),
    };

    let annotations = job.metadata.annotations.as_ref();
    let artifact_sha = annotations.and_then(|a| a.get("artifactSha")).cloned();
    let config_sha = annotations.and_then(|a| a.get("configSha")).cloned();

    let egg = ArchivedJobEgg {
        name: job_name.clone(),
        namespace: namespace.clone(),
        job_template_name: owner_ref.name.clone(),
        job_template_namespace: namespace.clone(),
        uid: job_uid.clone(),
        status,
        start_time,
        completion_time: comp_time_str,
        duration_seconds,
        logs,
        output_result_json: output.result_json,
        output_report_md: output.report_md,
        output_test_results_xml: output.test_results_xml,
        output_archive: output.archive,
        output_test_snapshots: output.test_snapshots,
        events_json: serde_json::to_string(&events).ok(),
        snapshot_status,
        snapshot_diff_json,
        artifact_sha,
        config_sha,
        snapshot_baseline_id,
        output_chips_json: output.chips_json,
        output_label: output.label,
        output_metrics_json: output.metrics_json,
    };

    // Archive to database
    let conn = pool
        .get()
        .map_err(|e| format!("Failed to get database connection: {}", e))?;
    ArchivedJob::upsert(&egg, &conn)
        .map_err(|e| format!("Failed to archive job {}/{}: {}", namespace, job_name, e))?;
    // Clean up temporary job output rows
    let _ = crate::db::job_output::delete_for_job(&job_name, &namespace, &conn);

    // Delete the job (and its pods) from kubernetes
    let job_api: Api<Job> = Api::namespaced(client.clone(), &namespace);
    let dp = DeleteParams {
        propagation_policy: Some(kube::api::PropagationPolicy::Background),
        ..Default::default()
    };
    job_api
        .delete(&job_name, &dp)
        .await
        .map_err(|e| format!("Failed to delete job {}/{}: {}", namespace, job_name, e))?;

    metrics.forget_job_completion(&job_uid);

    log::info!(
        "Successfully archived and deleted job {}/{}",
        namespace,
        job_name
    );
    Ok(())
}

fn error_policy(_job: Arc<Job>, error: &kube::Error, _ctx: Arc<Context>) -> Action {
    log::warn!("Reconcile error: {:?}", error);
    Action::requeue(Duration::from_secs(60))
}

/// Returns the time a job reached a terminal state (Succeeded or Failed).
fn terminal_time(job: &Job) -> Option<chrono::DateTime<Utc>> {
    let status = job.status.as_ref()?;

    // Check completion_time first (set on success)
    if let Some(ct) = &status.completion_time {
        return Some(ct.0);
    }

    // For failed jobs, look at conditions
    if let Some(conditions) = &status.conditions {
        for c in conditions {
            if c.type_ == "Failed"
                && c.status == "True"
                && let Some(t) = &c.last_transition_time
            {
                return Some(t.0);
            }
        }
    }

    None
}

fn job_status_string(job: &Job) -> String {
    let status = match job.status.as_ref() {
        Some(s) => s,
        None => return "Unknown".to_string(),
    };

    if let Some(conditions) = &status.conditions {
        for c in conditions {
            if c.type_ == "Complete" && c.status == "True" {
                return "Succeeded".to_string();
            }
            if c.type_ == "Failed" && c.status == "True" {
                return "Failed".to_string();
            }
        }
    }

    "Unknown".to_string()
}

async fn fetch_job_logs(client: &Client, namespace: &str, job_name: &str) -> Option<String> {
    // List pods with the job-name label
    let pod_api: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client.clone(), namespace);
    let pods = match pod_api
        .list(&kube::api::ListParams::default().labels(&format!("job-name={}", job_name)))
        .await
    {
        Ok(list) => list.items,
        Err(e) => {
            log::warn!(
                "Failed to list pods for job {}/{}: {}",
                namespace,
                job_name,
                e
            );
            return None;
        }
    };

    let mut all_logs = Vec::new();
    for pod in &pods {
        let pod_name = pod.name_any();
        match pod_api
            .logs(
                &pod_name,
                &LogParams {
                    timestamps: true,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(log_str) => {
                if pods.len() > 1 {
                    all_logs.push(format!("=== Pod: {} ===\n{}", pod_name, log_str));
                } else {
                    all_logs.push(log_str);
                }
            }
            Err(e) => {
                log::warn!("Failed to fetch logs for pod {}: {}", pod_name, e);
            }
        }
    }

    if all_logs.is_empty() {
        None
    } else {
        Some(all_logs.join("\n"))
    }
}

/// Parse a duration string like "1d", "5h", "30m", "1h30m", "2d12h", etc.
pub fn parse_duration(s: &str) -> chrono::Duration {
    let mut total_seconds: i64 = 0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else {
            let n: i64 = current_num.parse().unwrap_or(0);
            current_num.clear();
            match c {
                'd' => total_seconds += n * 86400,
                'h' => total_seconds += n * 3600,
                'm' => total_seconds += n * 60,
                's' => total_seconds += n,
                _ => {}
            }
        }
    }

    if total_seconds == 0 {
        log::warn!(
            "Invalid or empty cleanupAfter value '{}', defaulting to 30m",
            s
        );
        chrono::Duration::minutes(30)
    } else {
        chrono::Duration::seconds(total_seconds)
    }
}
