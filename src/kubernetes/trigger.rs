use k8s_openapi::api::batch::v1::Job;

/// Label recording how a Job was started. Set on the Job and its pod template.
pub const TRIGGER_LABEL: &str = "jobfather.coolkev.com/trigger";

/// How a Job was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobTrigger {
    /// Created by the scheduler because the template's schedule was due.
    Scheduled,
    /// Created on demand: the web UI's Run dialog, the run API, or the MCP `run_job` tool.
    Manual,
}

impl JobTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            JobTrigger::Scheduled => "scheduled",
            JobTrigger::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "scheduled" => Some(JobTrigger::Scheduled),
            "manual" => Some(JobTrigger::Manual),
            _ => None,
        }
    }

    /// The trigger recorded on a Job's labels. `None` for Jobs created before
    /// the label existed (or by something other than Jobfather).
    pub fn from_job(job: &Job) -> Option<Self> {
        job.metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(TRIGGER_LABEL))
            .and_then(|v| Self::parse(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for t in [JobTrigger::Scheduled, JobTrigger::Manual] {
            assert_eq!(JobTrigger::parse(t.as_str()), Some(t));
        }
        assert_eq!(JobTrigger::parse("cron"), None);
    }

    #[test]
    fn reads_label_from_job() {
        let mut job = Job::default();
        assert_eq!(JobTrigger::from_job(&job), None);
        job.metadata.labels = Some(
            [(TRIGGER_LABEL.to_string(), "manual".to_string())]
                .into_iter()
                .collect(),
        );
        assert_eq!(JobTrigger::from_job(&job), Some(JobTrigger::Manual));
    }
}
