use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use kube::api::{DeleteParams, LogParams};
use kube::runtime::controller::Action;
use kube::runtime::Controller;
use kube::{Api, Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::{archived_job::ArchivedJobEgg, ArchivedJob};
use crate::kubernetes::JobTemplate;
use crate::metrics::Metrics;

struct Context {
    client: Client,
    pool: Pool<SqliteConnectionManager>,
    metrics: Arc<Metrics>,
    /// UIDs of jobs for which completion metrics have already been recorded.
    metrics_recorded: Mutex<HashSet<String>>,
}

pub async fn run(client: Client, pool: Pool<SqliteConnectionManager>, metrics: Arc<Metrics>) {
    let job_api: Api<Job> = Api::all(client.clone());

    let ctx = Arc::new(Context {
        client: client.clone(),
        pool,
        metrics,
        metrics_recorded: Mutex::new(HashSet::new()),
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
    let owner_ref = match job
        .metadata
        .owner_references
        .as_ref()
        .and_then(|refs| refs.iter().find(|r| r.kind == "JobTemplate"))
    {
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

    // Record completion metrics once per job
    let job_uid = job.metadata.uid.clone().unwrap_or_default();
    {
        let mut recorded = ctx.metrics_recorded.lock().unwrap_or_else(|e| e.into_inner());
        if recorded.insert(job_uid.clone()) {
            let status = job_status_string(&job);
            let duration_seconds = job
                .status
                .as_ref()
                .and_then(|s| s.start_time.as_ref())
                .map(|t| (completion_time - t.0).num_seconds());
            ctx.metrics.record_job_completion(
                &namespace,
                &owner_ref.name,
                &status,
                duration_seconds,
            );
        }
        ctx.metrics
            .reconciler_tracked_jobs
            .set(recorded.len() as f64);
    }

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
        let remaining = (cleanup_duration - elapsed).to_std().unwrap_or(Duration::from_secs(30));
        return Ok(Action::requeue(remaining));
    }

    // Time to archive and delete
    log::info!("Archiving and deleting job {}/{}", namespace, job_name);

    // Fetch logs, events, and job output from the job's pod(s)
    let logs = fetch_job_logs(&ctx.client, &namespace, &job_name).await;
    let events = crate::kubernetes::events::fetch_job_events(&ctx.client, &namespace, &job).await;
    let output = crate::kubernetes::job_output::JobOutput::load(&job_name, &namespace, &ctx.pool);

    let status = job_status_string(&job);
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
    let (snapshot_status, snapshot_diff_json, snapshot_baseline_id) = match ctx.pool.get() {
        Ok(conn) => {
            let status = crate::db::job_output::get_string(&job_name, &namespace, "_snapshot_status", &conn);
            let diff = crate::db::job_output::get_string(&job_name, &namespace, "_snapshot_diff_json", &conn);
            let baseline_id = crate::db::job_output::get_string(&job_name, &namespace, "_snapshot_baseline_id", &conn)
                .and_then(|s| s.parse::<i64>().ok());
            (status, diff, baseline_id)
        }
        Err(_) => (None, None, None),
    };

    let annotations = job.metadata.annotations.as_ref();
    let artifact_sha = annotations
        .and_then(|a| a.get("artifactSha"))
        .cloned();
    let config_sha = annotations
        .and_then(|a| a.get("configSha"))
        .cloned();

    let egg = ArchivedJobEgg {
        name: job_name.clone(),
        namespace: namespace.clone(),
        job_template_name: owner_ref.name.clone(),
        job_template_namespace: namespace.clone(),
        uid: job.uid().unwrap_or_default(),
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
    };

    // Archive to database
    match ctx.pool.get() {
        Ok(conn) => {
            if let Err(e) = ArchivedJob::upsert(&egg, &conn) {
                log::error!("Failed to archive job {}/{}: {}", namespace, job_name, e);
                return Ok(Action::requeue(Duration::from_secs(30)));
            }
            // Clean up temporary job output rows
            let _ = crate::db::job_output::delete_for_job(&job_name, &namespace, &conn);
        }
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return Ok(Action::requeue(Duration::from_secs(30)));
        }
    }

    // Delete the job from kubernetes
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), &namespace);
    let dp = DeleteParams {
        propagation_policy: Some(kube::api::PropagationPolicy::Background),
        ..Default::default()
    };
    if let Err(e) = job_api.delete(&job_name, &dp).await {
        log::error!("Failed to delete job {}/{}: {}", namespace, job_name, e);
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    // Clean up metrics tracking for this job
    if let Ok(mut recorded) = ctx.metrics_recorded.lock() {
        recorded.remove(&job_uid);
    }

    log::info!("Successfully archived and deleted job {}/{}", namespace, job_name);
    Ok(Action::await_change())
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
            if c.type_ == "Failed" && c.status == "True"
                && let Some(t) = &c.last_transition_time {
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
            log::warn!("Failed to list pods for job {}/{}: {}", namespace, job_name, e);
            return None;
        }
    };

    let mut all_logs = Vec::new();
    for pod in &pods {
        let pod_name = pod.name_any();
        match pod_api
            .logs(&pod_name, &LogParams {
                timestamps: true,
                ..Default::default()
            })
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
        log::warn!("Invalid or empty cleanupAfter value '{}', defaulting to 30m", s);
        chrono::Duration::minutes(30)
    } else {
        chrono::Duration::seconds(total_seconds)
    }
}
