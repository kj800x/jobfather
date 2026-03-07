use std::sync::Arc;

use chrono::{DateTime, Utc};
use croner::Cron;
use k8s_openapi::api::batch::v1::Job;
use kube::api::PostParams;
use kube::{Api, Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

use crate::kubernetes::JobTemplate;
use crate::metrics::Metrics;

/// Resolve a schedule string (keyword or cron expression) into a validated cron expression.
fn resolve_schedule(schedule: &str, namespace: &str, name: &str) -> Option<String> {
    let cron_expr = match schedule.trim().to_lowercase().as_str() {
        "hourly" | "daily" | "weekly" | "monthly" => {
            let hash_input = format!("{}/{}", namespace, name);
            let digest = md5::compute(hash_input.as_bytes());
            let hash =
                u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]);

            match schedule.trim().to_lowercase().as_str() {
                "hourly" => format!("{} * * * *", hash % 60),
                "daily" => format!("{} {} * * *", hash % 60, (hash >> 8) % 24),
                "weekly" => format!(
                    "{} {} * * {}",
                    hash % 60,
                    (hash >> 8) % 24,
                    (hash >> 16) % 7
                ),
                "monthly" => format!(
                    "{} {} {} * *",
                    hash % 60,
                    (hash >> 8) % 24,
                    (hash >> 16) % 28 + 1
                ),
                _ => unreachable!(),
            }
        }
        _ => schedule.trim().to_string(),
    };

    // Validate the cron expression
    match Cron::new(&cron_expr).parse() {
        Ok(_) => Some(cron_expr),
        Err(e) => {
            log::warn!(
                "Invalid schedule '{}' (resolved to '{}') for {}/{}: {}",
                schedule,
                cron_expr,
                namespace,
                name,
                e
            );
            None
        }
    }
}

/// Determine if a cron schedule is due given the last run time.
fn is_schedule_due(
    cron_expr: &str,
    last_run_time: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    let cron = match Cron::new(cron_expr).parse() {
        Ok(c) => c,
        Err(_) => return false,
    };

    let prev_tick = find_prev_occurrence(&cron, now);

    let Some(prev_tick) = prev_tick else {
        return false;
    };

    match last_run_time {
        None => true,
        Some(last) => last < prev_tick,
    }
}

/// Find the most recent cron occurrence at or before `now`.
fn find_prev_occurrence(cron: &Cron, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    use chrono::Duration;

    let search_start = now - Duration::days(32);
    let mut current = search_start;
    let mut last_before_now = None;

    loop {
        match cron.find_next_occurrence(&current, false) {
            Ok(next) if next <= now => {
                last_before_now = Some(next);
                current = next + Duration::seconds(1);
            }
            _ => break,
        }
    }

    last_before_now
}

/// Get the most recent run time for a JobTemplate from both live and archived jobs.
async fn get_last_run_time(
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    namespace: &str,
    jt_name: &str,
    jt_uid: &str,
) -> Option<DateTime<Utc>> {
    // Live jobs: find jobs owned by this JobTemplate
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
    let live_max = match job_api
        .list(&kube::api::ListParams::default())
        .await
    {
        Ok(job_list) => job_list
            .items
            .iter()
            .filter(|j| {
                j.metadata
                    .owner_references
                    .as_ref()
                    .is_some_and(|refs| refs.iter().any(|r| r.uid == jt_uid))
            })
            .filter_map(|j| {
                j.status
                    .as_ref()
                    .and_then(|s| s.start_time.as_ref())
                    .map(|t| t.0)
            })
            .max(),
        Err(e) => {
            log::warn!("Failed to list live jobs for {}/{}: {}", namespace, jt_name, e);
            None
        }
    };

    // Archived jobs
    let archived_max = match pool.get() {
        Ok(conn) => {
            let jt_name = jt_name.to_string();
            let namespace = namespace.to_string();
            conn.query_row(
                "SELECT MAX(start_time) FROM archived_job WHERE job_template_name = ?1 AND job_template_namespace = ?2",
                params![jt_name, namespace],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        }
        Err(e) => {
            log::warn!("Failed to get DB connection for scheduler: {}", e);
            None
        }
    };

    match (live_max, archived_max) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

pub async fn run(client: Client, pool: Pool<SqliteConnectionManager>, metrics: Arc<Metrics>) {
    log::info!("Starting scheduler loop");
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        interval.tick().await;

        let tick_timer = metrics.scheduler_tick_duration_seconds.start_timer();

        let jt_api: Api<JobTemplate> = Api::all(client.clone());
        let job_templates = match jt_api.list(&kube::api::ListParams::default()).await {
            Ok(list) => list.items,
            Err(e) => {
                log::warn!("Scheduler: failed to list JobTemplates: {}", e);
                tick_timer.observe_duration();
                continue;
            }
        };

        let now = Utc::now();

        for jt in &job_templates {
            let schedule = match &jt.spec.schedule {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };

            let namespace = jt.metadata.namespace.as_deref().unwrap_or("default");
            let name = jt.metadata.name.as_deref().unwrap_or("unknown");
            let uid = jt.uid().unwrap_or_default();

            let cron_expr = match resolve_schedule(schedule, namespace, name) {
                Some(expr) => expr,
                None => continue,
            };

            let last_run = get_last_run_time(&client, &pool, namespace, name, &uid).await;

            if !is_schedule_due(&cron_expr, last_run, now) {
                continue;
            }

            // Create the job
            match super::job_create::build_job(&jt, None, None) {
                Ok(job) => {
                    let job_name = job.metadata.name.clone().unwrap_or_default();
                    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
                    match job_api.create(&PostParams::default(), &job).await {
                        Ok(_) => {
                            log::info!(
                                "Scheduler: created job {} for {}/{}",
                                job_name,
                                namespace,
                                name
                            );
                        }
                        Err(e) => {
                            log::warn!(
                                "Scheduler: failed to create job for {}/{}: {}",
                                namespace,
                                name,
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Scheduler: failed to build job for {}/{}: {}",
                        namespace,
                        name,
                        e
                    );
                }
            }
        }

        // Update gauge metrics
        metrics
            .update_gauges(&client, &pool, &job_templates)
            .await;

        tick_timer.observe_duration();
    }
}
