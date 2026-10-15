# Jobfather

Kubernetes job controller, since the out-of-the-box job system in Kubernetes is...
pretty limited.

## Features

- **Scheduled jobs** — cron expressions or keyword schedules (`hourly`, `daily`, `weekly`, `monthly`)
- **On-demand jobs** — run from the web UI with optional arg/env overrides
- **Job lifecycle** — live and archived job views with logs, events, and duration tracking
- **Job output** — structured output capture via `/job-output` volume (JSON, markdown, JUnit XML, archives)
- **Acceptance testing** — JUnit XML parsing, consecutive failure tracking, snapshot diffing with versioned baselines
- **Prometheus metrics** — job durations, completion counters, longest running, time-since-last-success, test case durations, scheduler tick timing
- **Automatic cleanup** — configurable retention via `cleanupAfter` (e.g. `1h30m`, `7d`, default `30m`)

## How it works

All jobs are specified as **JobTemplates** (a custom resource), which Jobfather instantiates into Kubernetes Jobs.

A JobTemplate provides:
- A pod spec
- An optional `schedule` for automatic execution
- An optional `acceptanceTest` flag for test-oriented features
- An optional `cleanupAfter` duration controlling how long completed jobs remain before archival

## Terminology

- **Live** — a Job that currently exists as a resource in the Kubernetes cluster
- **Archived** — a Job that has been cleaned up from Kubernetes; its final status and logs are preserved in SQLite

## Architecture

Jobfather runs three concurrent processes:

1. **Web UI** — an actix-web server providing a dashboard to view JobTemplates, live/archived jobs, logs, events, and job output
2. **Reconciler** — a kube-runtime controller that watches Jobs, records completion metrics, and after a job has been in a terminal state for longer than its template's `cleanupAfter` duration, archives its logs, output, and status to SQLite then deletes it from the cluster
3. **Scheduler** — polls every 30 seconds, evaluates cron schedules against last-run times (from both live K8s and archived DB), and creates jobs when due. Also updates Prometheus gauge metrics each tick.

## Job Output

Every job created by Jobfather gets an `emptyDir` volume mounted at `/job-output`. Jobs can write to this directory to produce structured output that Jobfather will capture, archive, and display in the web UI.

### Supported files

All files are optional. Write only the ones your job needs.

| Path | Format | Purpose |
|------|--------|---------|
| `/job-output/result.json` | JSON | Structured result data (e.g. metrics, summaries, key-value results) |
| `/job-output/report.md` | Markdown | Human-readable report |
| `/job-output/chips.json` | JSON | Ordered name/value chips shown on run listings, the job detail page, and the home page (see [Chips](#chips)) |
| `/job-output/label.txt` | Text | Human-friendly label for the run (e.g. `remux of config 10`), shown instead of the job name in the run listing and as the job detail title |
| `/job-output/metrics.json` | JSON | Gauges and counters re-exported on `/api/metrics` (see [Job output metrics](#job-output-metrics)) |
| `/job-output/test-results.xml` | JUnit XML | Test results for acceptance test jobs |
| `/job-output/archive.tar.gz` | gzip tarball | Escape hatch for arbitrary output files |
| `/job-output/test-snapshots/` | Directory | Snapshot files for baseline comparison; automatically tarred and uploaded as `test-snapshots.tar.gz` |

### How it works

1. Jobfather injects an `emptyDir` volume, `/job-output` mount, and a **sidecar container** on every job it creates
2. Your job writes files to `/job-output` during execution
3. When the main container exits, Kubernetes sends SIGTERM to the sidecar
4. The sidecar uploads each found output file to Jobfather's API (`PUT /api/jobs/{ns}/{name}/output/{filename}`)
5. On cleanup, the reconciler copies output from the upload table to the archived job record, then deletes the temporary rows
6. The web UI displays each output type: JSON and XML in code blocks, markdown rendered as formatted HTML (raw HTML in the markdown is escaped), and the archive as a download link

The sidecar uses `curlimages/curl` and is implemented as a native Kubernetes sidecar container (`initContainers` with `restartPolicy: Always`, requires K8s 1.28+).

### Requirements

Set the `JOBFATHER_URL` environment variable on the Jobfather deployment to the cluster-internal URL (e.g. `http://jobfather.cicd.svc:8080`). If unset, the sidecar is not injected and job output capture is silently skipped. Logs and events are still captured at archival time.

## Chips

Jobs can write `/job-output/chips.json` to show small name/value tags for each run. The file is a JSON array, and chips are shown in array order:

```json
[
  { "name": "coverage", "value": 87.3, "color": "success" },
  { "name": "env", "value": "staging" },
  { "name": "flaky", "value": true, "color": "warning" },
  { "name": "build", "value": "a1b2c3", "color": "#8250df" }
]
```

- `name` (required): string
- `value` (required): string, number, or boolean
- `color` (optional, default `neutral`): one of `neutral`, `info`, `success`, `warning`, `danger`, or a hex code (`#rgb` / `#rrggbb`). Text color is chosen automatically for hex backgrounds.

Chips appear in a column on the job template's run listing, in the job detail info card, and on the home page for each template's most recent run (once it finishes). Parsing is lenient: invalid entries are skipped, unknown colors fall back to `neutral`, and the job detail page lists what was skipped.

## Snapshots

Acceptance test jobs can write files to `/job-output/test-snapshots/`. On upload, Jobfather:
1. Extracts the tarball and compares each file against the latest baseline
2. Records a status (`matches_baseline` or `differs_from_baseline`) and a JSON diff
3. Displays file-level diffs in the job detail page (unified diffs for JSON files)

Users can **accept snapshots as a new baseline** from the UI, which creates a versioned baseline set. Previous baselines are preserved for reference.

## Metrics

Prometheus metrics are served at `GET /api/metrics`. Time-since gauges are refreshed at scrape time for accuracy.

| Metric | Type | Description |
|--------|------|-------------|
| `jobfather_job_duration_seconds` | Histogram | Duration of completed jobs |
| `jobfather_job_completions_total` | Counter | Completed jobs by namespace/template/status |
| `jobfather_job_longest_running_seconds` | Gauge | Duration of the longest currently running job per template |
| `jobfather_time_since_last_completion_seconds` | Gauge | Seconds since last job completion (any status) |
| `jobfather_time_since_last_success_seconds` | Gauge | Seconds since last successful job completion |
| `jobfather_acceptance_consecutive_failures` | Gauge | Consecutive failing runs for acceptance test templates |
| `jobfather_test_case_duration_seconds` | Gauge | Individual test case durations from JUnit XML |
| `jobfather_scheduler_tick_duration_seconds` | Histogram | Scheduler loop iteration duration |
| `jobfather_reconciler_tracked_jobs` | Gauge | Number of job UIDs in the reconciler's metrics tracking set |
| `jobfather_job_output_gauge` | Gauge | Gauges reported by jobs via `metrics.json` (see below) |
| `jobfather_job_output_gauge_timestamp_seconds` | Gauge | Unix time each job output gauge was last reported |
| `jobfather_job_output_total` | Counter | Counters reported by jobs via `metrics.json` (see below) |

### Job output metrics

Jobs can write `/job-output/metrics.json` to feed values into Prometheus/VictoriaMetrics through Jobfather's `/api/metrics` endpoint:

```json
[
  { "name": "queue_depth", "value": 17 },
  { "name": "latency_p95_seconds", "value": 0.42, "labels": { "endpoint": "/api" } },
  { "name": "rows_processed", "value": 12345, "type": "counter" }
]
```

- `name` (required): any string; exported as the `name` label
- `value` (required): finite number
- `type` (optional, default `gauge`): `gauge` or `counter`
- `labels` (optional): extra labels. Names must be valid Prometheus label names; `namespace`, `job_template`, and `name` are reserved.

Every series gets `namespace`, `job_template`, and `name` labels, e.g. `jobfather_job_output_gauge{namespace="ns", job_template="etl", name="queue_depth"}`.

- **Gauges** hold the value from the most recent run that reported that series (regardless of run status). `jobfather_job_output_gauge_timestamp_seconds` tells you when it was reported, so stale values can be detected. A series that a job stops reporting keeps its last value.
- **Counters** add each run's value (which must be non-negative) to a running total, so `increase()` / `rate()` give totals over time (e.g. rows processed per day).

Values are recorded when the sidecar uploads `metrics.json` and persisted in SQLite, so they survive Jobfather restarts. The job detail page shows the metrics each run reported.

## Development

Jobfather is designed to run inside the cluster, but during development you can run it on your laptop. Since pods can't directly reach your laptop's IP, set up a reverse SSH tunnel through a cluster node:

```bash
ssh -R 0.0.0.0:8080:localhost:8080 <node-ip>
```

Then set `JOBFATHER_URL=http://<node-ip>:8080`. This requires `GatewayPorts yes` in the node's `/etc/ssh/sshd_config`.

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PORT` | `8080` | HTTP server port |
| `DATABASE_PATH` | `db.db` | SQLite database file path |
| `JOBFATHER_URL` | *(unset)* | Cluster-internal URL for sidecar output uploads; if unset, sidecar is not injected |
