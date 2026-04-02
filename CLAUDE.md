# Jobfather

Kubernetes job controller with a web UI. Built with Rust, actix-web, maud (HTML templating), htmx, and kube-rs.

## Terminology

- **Live** — a Job currently exists as a Kubernetes resource in the cluster
- **Archived** — a Job that has been cleaned up from Kubernetes; its final status and logs are stored in SQLite

Use "live" and "archived" consistently in code, UI labels, comments, and documentation. Do not use "active", "running" (as a source label), or "history" as substitutes for these terms.

## Job output

Every job gets an `emptyDir` mounted at `/job-output`. Supported files:
- `/job-output/result.json` — structured JSON result data
- `/job-output/report.md` — human-readable markdown report
- `/job-output/test-results.xml` — JUnit XML test results
- `/job-output/archive.tar.gz` — escape hatch for arbitrary files
- `/job-output/test-snapshots/` — directory of snapshot files; automatically tarred and uploaded as `test-snapshots.tar.gz`

Files are uploaded by a sidecar container (`curlimages/curl`) that receives SIGTERM when the main container exits, then PUTs files to Jobfather's API. Requires `JOBFATHER_URL` env var to be set on the Jobfather deployment.

## Snapshots

Acceptance test jobs can write files to `/job-output/test-snapshots/`. On upload, Jobfather compares them against the latest baseline and records a diff status. Users can accept new snapshots as baseline from the UI, which versions the baseline set. Snapshot diffs for JSON files are displayed as unified diffs in the job detail page.

## Scheduling

JobTemplates with a `schedule` field are automatically run by the scheduler. Supports:
- Cron expressions (e.g. `*/5 * * * *`)
- Keywords: `hourly`, `daily`, `weekly`, `monthly` — resolved to deterministic cron times via MD5 hash of `namespace/name`

The scheduler polls every 30 seconds, checks the last run time (from both live K8s jobs and archived DB), and creates a new job if the schedule is due.

## Metrics

Prometheus metrics are served at `/api/metrics`. Key metrics:
- `jobfather_job_duration_seconds` — histogram of completed job durations
- `jobfather_job_completions_total` — counter by namespace/template/status
- `jobfather_job_longest_running_seconds` — gauge per template
- `jobfather_time_since_last_completion_seconds` / `..._success_seconds` — gauges updated at scrape time
- `jobfather_acceptance_consecutive_failures` — gauge for acceptance test templates (considers AT result, not just termination)
- `jobfather_job_consecutive_failures` — gauge of consecutive non-Succeeded runs per template (all templates)
- `jobfather_job_last_status` — one-hot gauge with `status` label for each template's most recent run
- `jobfather_test_case_duration_seconds` — gauge per test case from JUnit XML
- `jobfather_scheduler_tick_duration_seconds` — histogram of scheduler loop iterations
- `jobfather_reconciler_tracked_jobs` — gauge of UIDs in the reconciler's metrics tracking set

## Project structure

- `src/main.rs` — entrypoint, runs web server, reconciler, and scheduler concurrently via `tokio::select!`
- `src/kubernetes/` — CRD types (`JobTemplate`), reconciler, scheduler, job creation, event fetching, job output collection
- `src/web/` — actix-web routes and maud templates (job templates list, job template detail, job detail, snapshot API, job output API)
- `src/db/` — SQLite schema, migrations, `ArchivedJob` model, job output temp storage, snapshot baselines
- `src/metrics.rs` — Prometheus metric definitions and update logic
- `src/snapshot.rs` — tarball extraction, baseline comparison, JSON diffing
- `src/lib.rs` — static file serving macro
- `src/res/` — static assets (CSS, JS: htmx, idiomorph)
- `kubernetes/` — CRD YAML and example manifests
