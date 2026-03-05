# Jobfather

Kubernetes job controller, since the out-of-the-box job system in Kubernetes is...
pretty limited.

## Features

- Cronjobs
- On demand jobs
- Job history, logs, etc.
- Jobs as test suites
- Metrics and alerting

All jobs are specified as JobTemplates, which Jobfather will instantiate into Jobs.

A job template provides:
- The job spec
- An optional schedule for automatically running the job
- Optional config to treat the job as an acceptance test

When launched on-demand
