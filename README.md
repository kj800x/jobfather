# Jobfather

Kubernetes job controller, since the out-of-the-box job system in Kubernetes is...
pretty limited.

## Features

- Cronjobs
- On demand jobs
- Job history, logs, etc.
- Jobs as test suites
- Metrics and alerting
- Automatic cleanup of completed jobs with configurable retention (`cleanupAfter`)
- Job output capture via `/job-output` volume

All jobs are specified as JobTemplates, which Jobfather will instantiate into Jobs.

A job template provides:
- The job spec (PodSpec)
- An optional schedule for automatically running the job
- Optional config to treat the job as an acceptance test
- Optional `cleanupAfter` duration (e.g. `1h30m`, `7d`) — defaults to `30m`

## Terminology

- **Live** — a Job that currently exists as a resource in the Kubernetes cluster
- **Archived** — a Job that has been cleaned up from Kubernetes; its final status and logs are preserved in the local database

## Architecture

Jobfather runs two concurrent processes:

1. **Web UI** — an actix-web server providing a dashboard to view JobTemplates, live/archived jobs, and their logs
2. **Reconciler** — a kube-runtime controller that watches Jobs, and after a job has been in a terminal state for longer than its template's `cleanupAfter` duration, archives its logs, output files, and status to SQLite then deletes it from the cluster

## Job Output

Every job created by Jobfather gets an `emptyDir` volume mounted at `/job-output`. Jobs can write to this directory to produce structured output that Jobfather will capture, archive, and display in the web UI.

### Supported files

All files are optional. Write only the ones your job needs.

| Path | Format | Purpose |
|------|--------|---------|
| `/job-output/result.json` | JSON | Structured result data (e.g. metrics, summaries, key-value results) |
| `/job-output/report.md` | Markdown | Human-readable report |
| `/job-output/test-results.xml` | JUnit XML | Test results for acceptance test jobs |
| `/job-output/archive.tar.gz` | gzip tarball | Escape hatch for arbitrary output that doesn't fit the above formats |

### How it works

1. Jobfather injects an `emptyDir` volume, `/job-output` mount, and a **sidecar container** on every job it creates
2. Your job writes files to `/job-output` during execution
3. When the main container exits, Kubernetes sends SIGTERM to the sidecar
4. The sidecar uploads each found output file to Jobfather's API (`PUT /api/jobs/{ns}/{name}/output/{filename}`)
5. On cleanup, the reconciler copies output from the upload table to the archived job record, then deletes the temporary rows
6. The web UI displays each output type: JSON and XML in code blocks, markdown as preformatted text, and the archive as a download link

The sidecar uses `curlimages/curl` and is implemented as a native Kubernetes sidecar container (`initContainers` with `restartPolicy: Always`, requires K8s 1.28+).

### Requirements

Set the `JOBFATHER_URL` environment variable on the Jobfather deployment to the cluster-internal URL (e.g. `http://jobfather.cicd.svc:8080`). If unset, the sidecar is not injected and job output capture is silently skipped. Logs and events are unaffected.

## Development

Jobfather is designed to run inside the cluster, but during development you can run it on your laptop. Since pods can't directly reach your laptop's IP, set up a reverse SSH tunnel through a cluster node:

```bash
ssh -R 0.0.0.0:8080:localhost:8080 <node-ip>
```

Then set `JOBFATHER_URL=http://<node-ip>:8080`. This requires `GatewayPorts yes` in the node's `/etc/ssh/sshd_config`.
