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

## Project structure

- `src/main.rs` — entrypoint, runs web server and reconciler concurrently
- `src/kubernetes/` — CRD types (`JobTemplate`), reconciler, and job output collection
- `src/web/` — actix-web routes and maud templates
- `src/db/` — SQLite schema, migrations, and `ArchivedJob` model
- `src/res/` — static assets (CSS, JS)
- `kubernetes/` — CRD YAML and example manifests
