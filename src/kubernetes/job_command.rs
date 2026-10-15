use k8s_openapi::api::batch::v1::Job;
use serde::{Deserialize, Serialize};

/// The command and args a job's container ran with, as recorded in its pod spec.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContainerCommand {
    pub container: String,
    /// Empty when the container uses the image's entrypoint.
    pub command: Vec<String>,
    pub args: Vec<String>,
}

impl ContainerCommand {
    /// Shell-quoted command line, or None if the container uses the image's
    /// entrypoint with no args.
    pub fn display(&self) -> Option<String> {
        let words: Vec<&str> = self
            .command
            .iter()
            .chain(self.args.iter())
            .map(String::as_str)
            .collect();
        if words.is_empty() {
            None
        } else {
            Some(shell_words::join(words))
        }
    }
}

/// Commands of the job's main containers. The job-output sidecar is an init
/// container, so it is not included.
pub fn container_commands(job: &Job) -> Vec<ContainerCommand> {
    job.spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .map(|pod| {
            pod.containers
                .iter()
                .map(|c| ContainerCommand {
                    container: c.name.clone(),
                    command: c.command.clone().unwrap_or_default(),
                    args: c.args.clone().unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_words() {
        let c = ContainerCommand {
            container: "main".to_string(),
            command: vec!["sh".to_string(), "-c".to_string()],
            args: vec!["echo hello && exit 1".to_string()],
        };
        assert_eq!(c.display().unwrap(), "sh -c 'echo hello && exit 1'");
    }

    #[test]
    fn display_is_none_for_image_entrypoint() {
        let c = ContainerCommand {
            container: "main".to_string(),
            command: vec![],
            args: vec![],
        };
        assert_eq!(c.display(), None);
    }
}
