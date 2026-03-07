use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use kube::{Api, Client, ResourceExt};
use prometheus::{
    register_gauge_vec_with_registry, register_gauge_with_registry,
    register_histogram_vec_with_registry, register_histogram_with_registry,
    register_int_counter_vec_with_registry, Gauge, GaugeVec, Histogram, HistogramOpts,
    HistogramVec, IntCounterVec, Opts, Registry,
};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

use crate::kubernetes::JobTemplate;
use crate::web::junit::parse_junit_xml;

pub struct Metrics {
    pub job_duration_seconds: HistogramVec,
    pub job_completions_total: IntCounterVec,
    pub job_longest_running_seconds: GaugeVec,
    pub time_since_last_completion_seconds: GaugeVec,
    pub time_since_last_success_seconds: GaugeVec,
    pub acceptance_consecutive_failures: GaugeVec,
    pub test_case_duration_seconds: GaugeVec,
    pub scheduler_tick_duration_seconds: Histogram,
    pub reconciler_tracked_jobs: Gauge,
}

impl Metrics {
    pub fn new(registry: &Registry) -> Self {
        let job_duration_seconds = register_histogram_vec_with_registry!(
            HistogramOpts::new(
                "jobfather_job_duration_seconds",
                "Duration of completed jobs in seconds"
            )
            .buckets(vec![1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0]),
            &["namespace", "job_template"],
            registry
        )
        .expect("Failed to register job_duration_seconds");

        let job_completions_total = register_int_counter_vec_with_registry!(
            Opts::new(
                "jobfather_job_completions_total",
                "Total number of completed jobs"
            ),
            &["namespace", "job_template", "status"],
            registry
        )
        .expect("Failed to register job_completions_total");

        let job_longest_running_seconds = register_gauge_vec_with_registry!(
            Opts::new(
                "jobfather_job_longest_running_seconds",
                "Duration in seconds of the longest currently running job instance"
            ),
            &["namespace", "job_template"],
            registry
        )
        .expect("Failed to register job_longest_running_seconds");

        let time_since_last_completion_seconds = register_gauge_vec_with_registry!(
            Opts::new(
                "jobfather_time_since_last_completion_seconds",
                "Seconds since the last job completion (any status)"
            ),
            &["namespace", "job_template"],
            registry
        )
        .expect("Failed to register time_since_last_completion_seconds");

        let time_since_last_success_seconds = register_gauge_vec_with_registry!(
            Opts::new(
                "jobfather_time_since_last_success_seconds",
                "Seconds since the last successful job completion"
            ),
            &["namespace", "job_template"],
            registry
        )
        .expect("Failed to register time_since_last_success_seconds");

        let acceptance_consecutive_failures = register_gauge_vec_with_registry!(
            Opts::new(
                "jobfather_acceptance_consecutive_failures",
                "Number of consecutive failing runs for acceptance test templates (0 = last run passed)"
            ),
            &["namespace", "job_template"],
            registry
        )
        .expect("Failed to register acceptance_consecutive_failures");

        let test_case_duration_seconds = register_gauge_vec_with_registry!(
            Opts::new(
                "jobfather_test_case_duration_seconds",
                "Duration of individual test cases from the most recent run"
            ),
            &["namespace", "job_template", "test_suite", "test_case"],
            registry
        )
        .expect("Failed to register test_case_duration_seconds");

        let reconciler_tracked_jobs = register_gauge_with_registry!(
            Opts::new(
                "jobfather_reconciler_tracked_jobs",
                "Number of job UIDs currently tracked in the reconciler metrics_recorded set"
            ),
            registry
        )
        .expect("Failed to register reconciler_tracked_jobs");

        let scheduler_tick_duration_seconds = register_histogram_with_registry!(
            HistogramOpts::new(
                "jobfather_scheduler_tick_duration_seconds",
                "Duration of each scheduler loop iteration"
            )
            .buckets(vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0]),
            registry
        )
        .expect("Failed to register scheduler_tick_duration_seconds");

        Self {
            job_duration_seconds,
            job_completions_total,
            job_longest_running_seconds,
            time_since_last_completion_seconds,
            time_since_last_success_seconds,
            acceptance_consecutive_failures,
            test_case_duration_seconds,
            scheduler_tick_duration_seconds,
            reconciler_tracked_jobs,
        }
    }

    /// Record a job completion event (called from the reconciler at archival time).
    pub fn record_job_completion(
        &self,
        namespace: &str,
        job_template: &str,
        status: &str,
        duration_seconds: Option<i64>,
    ) {
        self.job_completions_total
            .with_label_values(&[namespace, job_template, status])
            .inc();

        if let Some(dur) = duration_seconds {
            self.job_duration_seconds
                .with_label_values(&[namespace, job_template])
                .observe(dur as f64);
        }
    }

    /// Update all gauge metrics. Called periodically from the scheduler loop.
    pub async fn update_gauges(
        &self,
        client: &Client,
        pool: &Pool<SqliteConnectionManager>,
        job_templates: &[JobTemplate],
    ) {
        let now = Utc::now();

        // Reset longest running gauges before recomputing
        self.job_longest_running_seconds.reset();

        // Compute longest running job per template from live K8s jobs
        self.update_longest_running(client, job_templates, now).await;

        // Update acceptance metrics from DB + live K8s jobs (time-since gauges are updated on scrape)
        if let Ok(conn) = pool.get() {
            self.update_acceptance_metrics(&conn, client, job_templates).await;
        }
    }

    async fn update_longest_running(
        &self,
        client: &Client,
        job_templates: &[JobTemplate],
        now: DateTime<Utc>,
    ) {
        // Group templates by namespace to minimize API calls
        let mut namespaces: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for jt in job_templates {
            if let Some(ns) = jt.metadata.namespace.as_deref() {
                namespaces.insert(ns);
            }
        }

        // Build a map of (namespace, template_uid) -> template_name for owner ref matching
        let mut uid_to_template: std::collections::HashMap<String, (&str, &str)> =
            std::collections::HashMap::new();
        for jt in job_templates {
            let ns = jt.metadata.namespace.as_deref().unwrap_or("default");
            let name = jt.metadata.name.as_deref().unwrap_or("unknown");
            if let Some(uid) = jt.metadata.uid.as_deref() {
                uid_to_template.insert(uid.to_string(), (ns, name));
            }
        }

        for ns in namespaces {
            let job_api: Api<Job> = Api::namespaced(client.clone(), ns);
            let jobs = match job_api.list(&kube::api::ListParams::default()).await {
                Ok(list) => list.items,
                Err(_) => continue,
            };

            // Track longest running per template
            let mut longest: std::collections::HashMap<&str, f64> =
                std::collections::HashMap::new();

            for job in &jobs {
                // Only consider non-terminal jobs
                if job
                    .status
                    .as_ref()
                    .and_then(|s| s.completion_time.as_ref())
                    .is_some()
                {
                    continue;
                }
                if job
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .is_some_and(|conds| {
                        conds
                            .iter()
                            .any(|c| c.type_ == "Failed" && c.status == "True")
                    })
                {
                    continue;
                }

                let owner_uid = job
                    .metadata
                    .owner_references
                    .as_ref()
                    .and_then(|refs| refs.iter().find(|r| r.kind == "JobTemplate"))
                    .map(|r| r.uid.as_str());

                let Some(owner_uid) = owner_uid else {
                    continue;
                };
                let Some((_, template_name)) = uid_to_template.get(owner_uid) else {
                    continue;
                };

                let start = job
                    .status
                    .as_ref()
                    .and_then(|s| s.start_time.as_ref())
                    .map(|t| t.0);

                if let Some(start) = start {
                    let running_secs = (now - start).num_seconds() as f64;
                    let entry = longest.entry(template_name).or_insert(0.0);
                    if running_secs > *entry {
                        *entry = running_secs;
                    }
                }
            }

            for (template_name, duration) in &longest {
                self.job_longest_running_seconds
                    .with_label_values(&[ns, template_name])
                    .set(*duration);
            }
        }
    }

    /// Update time-since gauges from both archived DB and live K8s jobs.
    /// Called on each metrics scrape for accuracy.
    pub async fn update_time_since_gauges(
        &self,
        conn: &r2d2::PooledConnection<SqliteConnectionManager>,
        client: &Client,
        now: DateTime<Utc>,
    ) {
        // Collect last completion times from archived jobs
        let mut last_completion: std::collections::HashMap<(String, String), DateTime<Utc>> =
            std::collections::HashMap::new();
        let mut last_success: std::collections::HashMap<(String, String), DateTime<Utc>> =
            std::collections::HashMap::new();

        if let Ok(mut stmt) = conn.prepare(
            "SELECT job_template_name, job_template_namespace, MAX(completion_time)
             FROM archived_job
             GROUP BY job_template_name, job_template_namespace",
        )
            && let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            }) {
                for row in rows.flatten() {
                    if let Some(time_str) = row.2
                        && let Ok(time) = time_str.parse::<DateTime<Utc>>() {
                            last_completion.insert((row.0, row.1), time);
                        }
                }
            }

        if let Ok(mut stmt) = conn.prepare(
            "SELECT job_template_name, job_template_namespace, MAX(completion_time)
             FROM archived_job
             WHERE status = 'Succeeded'
             GROUP BY job_template_name, job_template_namespace",
        )
            && let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            }) {
                for row in rows.flatten() {
                    if let Some(time_str) = row.2
                        && let Ok(time) = time_str.parse::<DateTime<Utc>>() {
                            last_success.insert((row.0, row.1), time);
                        }
                }
            }

        // Also check live K8s jobs for completion times not yet archived
        let job_api: Api<Job> = Api::all(client.clone());
        if let Ok(job_list) = job_api.list(&kube::api::ListParams::default()).await {
            for job in &job_list.items {
                let owner = job
                    .metadata
                    .owner_references
                    .as_ref()
                    .and_then(|refs| refs.iter().find(|r| r.kind == "JobTemplate"));
                let Some(owner) = owner else { continue };

                let ns = job.metadata.namespace.as_deref().unwrap_or("default");
                let key = (owner.name.clone(), ns.to_string());

                // Check if job has a completion time
                let comp_time = job
                    .status
                    .as_ref()
                    .and_then(|s| s.completion_time.as_ref())
                    .map(|t| t.0);

                if let Some(ct) = comp_time {
                    let entry = last_completion.entry(key.clone()).or_insert(ct);
                    if ct > *entry {
                        *entry = ct;
                    }

                    // Check if it succeeded
                    let succeeded = job
                        .status
                        .as_ref()
                        .and_then(|s| s.conditions.as_ref())
                        .is_some_and(|conds| {
                            conds
                                .iter()
                                .any(|c| c.type_ == "Complete" && c.status == "True")
                        });
                    if succeeded {
                        let entry = last_success.entry(key).or_insert(ct);
                        if ct > *entry {
                            *entry = ct;
                        }
                    }
                }
            }
        }

        for ((template, namespace), time) in &last_completion {
            let elapsed = (now - *time).num_seconds() as f64;
            self.time_since_last_completion_seconds
                .with_label_values(&[namespace, template])
                .set(elapsed);
        }

        for ((template, namespace), time) in &last_success {
            let elapsed = (now - *time).num_seconds() as f64;
            self.time_since_last_success_seconds
                .with_label_values(&[namespace, template])
                .set(elapsed);
        }
    }

    async fn update_acceptance_metrics(
        &self,
        conn: &r2d2::PooledConnection<SqliteConnectionManager>,
        client: &Client,
        job_templates: &[JobTemplate],
    ) {
        // Reset test case durations to clear stale entries from renamed/removed tests
        self.test_case_duration_seconds.reset();

        // Fetch all live jobs once for use across all templates
        let job_api: Api<Job> = Api::all(client.clone());
        let live_jobs = job_api
            .list(&kube::api::ListParams::default())
            .await
            .map(|l| l.items)
            .unwrap_or_default();

        for jt in job_templates {
            if !jt.is_acceptance_test() {
                continue;
            }

            let ns = jt.metadata.namespace.as_deref().unwrap_or("default");
            let name = jt.metadata.name.as_deref().unwrap_or("unknown");
            let jt_uid = jt.uid().unwrap_or_default();

            // Count consecutive failures (None = DB error, skip update)
            if let Some(consecutive) =
                self.count_consecutive_failures(conn, name, ns, &jt_uid, &live_jobs)
            {
                self.acceptance_consecutive_failures
                    .with_label_values(&[ns, name])
                    .set(consecutive as f64);
            }

            // Update test case durations from most recent run with junit XML
            self.update_test_case_durations(conn, name, ns, &live_jobs, &jt_uid);
        }
    }

    fn count_consecutive_failures(
        &self,
        conn: &r2d2::PooledConnection<SqliteConnectionManager>,
        jt_name: &str,
        jt_namespace: &str,
        jt_uid: &str,
        live_jobs: &[Job],
    ) -> Option<u64> {
        // Collect results from archived jobs
        let mut stmt = match conn.prepare(
            "SELECT status, output_test_results_xml, snapshot_status, start_time
             FROM archived_job
             WHERE job_template_name = ?1 AND job_template_namespace = ?2
             ORDER BY start_time DESC
             LIMIT 100",
        ) {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "Failed to query consecutive failures for {}/{}: {}",
                    jt_namespace,
                    jt_name,
                    e
                );
                return None;
            }
        };

        let mut results: Vec<(String, Option<String>, Option<String>, String)> = stmt
            .query_map(params![jt_name, jt_namespace], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .ok()
            .map(|r| r.flatten().collect())
            .unwrap_or_default();

        // Collect results from live completed jobs owned by this template
        for job in live_jobs {
            let owner = job
                .metadata
                .owner_references
                .as_ref()
                .and_then(|refs| refs.iter().find(|r| r.kind == "JobTemplate"));
            let Some(owner) = owner else { continue };
            if owner.uid != jt_uid {
                continue;
            }

            // Only include completed jobs
            let Some(completion_time) = job
                .status
                .as_ref()
                .and_then(|s| s.completion_time.as_ref())
            else {
                continue;
            };

            let start_time = job
                .status
                .as_ref()
                .and_then(|s| s.start_time.as_ref())
                .map(|t| t.0.to_rfc3339())
                .unwrap_or_else(|| completion_time.0.to_rfc3339());

            let job_name = job.metadata.name.as_deref().unwrap_or("");
            let job_ns = job.metadata.namespace.as_deref().unwrap_or("default");

            // Determine status from K8s conditions
            let status = job
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .and_then(|conds| {
                    for c in conds {
                        if c.type_ == "Complete" && c.status == "True" {
                            return Some("Succeeded".to_string());
                        }
                        if c.type_ == "Failed" && c.status == "True" {
                            return Some("Failed".to_string());
                        }
                    }
                    None
                })
                .unwrap_or_else(|| "Unknown".to_string());

            // Load junit XML and snapshot status from job_output table
            let junit_xml = crate::db::job_output::get_string(job_name, job_ns, "test-results.xml", conn);
            let snapshot_status =
                crate::db::job_output::get_string(job_name, job_ns, "_snapshot_status", conn);

            results.push((status, junit_xml, snapshot_status, start_time));
        }

        // Sort by start_time descending (most recent first)
        results.sort_by(|a, b| b.3.cmp(&a.3));

        let mut count: u64 = 0;
        for (status, junit_xml, snapshot_status, _) in &results {
            if is_acceptance_failure(status, junit_xml.as_deref(), snapshot_status.as_deref()) {
                count += 1;
            } else {
                break;
            }
        }
        Some(count)
    }

    fn update_test_case_durations(
        &self,
        conn: &r2d2::PooledConnection<SqliteConnectionManager>,
        jt_name: &str,
        jt_namespace: &str,
        live_jobs: &[Job],
        jt_uid: &str,
    ) {
        // Find the most recent junit XML across archived and live jobs
        let archived_xml: Option<(String, String)> = conn
            .query_row(
                "SELECT output_test_results_xml, start_time FROM archived_job
                 WHERE job_template_name = ?1 AND job_template_namespace = ?2
                   AND output_test_results_xml IS NOT NULL
                 ORDER BY start_time DESC LIMIT 1",
                params![jt_name, jt_namespace],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .ok();

        // Check live completed jobs for more recent junit XML
        let mut best_xml: Option<String> = None;
        let mut best_start: Option<String> = None;

        if let Some((xml, start)) = &archived_xml {
            best_xml = Some(xml.clone());
            best_start = Some(start.clone());
        }

        for job in live_jobs {
            let owner = job
                .metadata
                .owner_references
                .as_ref()
                .and_then(|refs| refs.iter().find(|r| r.kind == "JobTemplate"));
            let Some(owner) = owner else { continue };
            if owner.uid != jt_uid {
                continue;
            }
            // Only completed jobs
            if job.status.as_ref().and_then(|s| s.completion_time.as_ref()).is_none() {
                continue;
            }

            let start_time = job
                .status
                .as_ref()
                .and_then(|s| s.start_time.as_ref())
                .map(|t| t.0.to_rfc3339());

            let job_name = job.metadata.name.as_deref().unwrap_or("");
            let job_ns = job.metadata.namespace.as_deref().unwrap_or("default");

            if let Some(xml) = crate::db::job_output::get_string(job_name, job_ns, "test-results.xml", conn) {
                let is_newer = match (&start_time, &best_start) {
                    (Some(live_st), Some(best_st)) => live_st > best_st,
                    (Some(_), None) => true,
                    _ => false,
                };
                if is_newer {
                    best_xml = Some(xml);
                    best_start = start_time;
                }
            }
        }

        let Some(xml) = best_xml else { return };
        let Some(suites) = parse_junit_xml(&xml) else {
            return;
        };

        for suite in &suites.suites {
            for case in &suite.cases {
                if let Some(time_str) = &case.time
                    && let Ok(duration) = time_str.parse::<f64>() {
                        self.test_case_duration_seconds
                            .with_label_values(&[jt_namespace, jt_name, &suite.name, &case.name])
                            .set(duration);
                    }
            }
        }
    }
}

/// Determine if an acceptance test run counts as a failure.
fn is_acceptance_failure(
    status: &str,
    junit_xml: Option<&str>,
    snapshot_status: Option<&str>,
) -> bool {
    // Container exited nonzero
    if status != "Succeeded" {
        return true;
    }

    // No junit.xml reported
    let Some(xml) = junit_xml else {
        return true;
    };

    // junit.xml has failures or errors
    if let Some(suites) = parse_junit_xml(xml) {
        if !suites.all_passed() {
            return true;
        }
    } else {
        // Couldn't parse junit XML — treat as failure
        return true;
    }

    // Snapshots differ from baseline (and not accepted)
    if let Some(ss) = snapshot_status
        && ss == "differs_from_baseline" {
            return true;
        }

    false
}
