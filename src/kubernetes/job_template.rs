use kube::CustomResource;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize)]
#[kube(
    kind = "JobTemplate",
    shortname = "jt",
    group = "jobfather.coolkev.com",
    version = "v1",
    namespaced,
    schema = "disabled",
    status = "JobTemplateStatus",
    printcolumn = r#"{"name":"Schedule", "jsonPath":".spec.schedule", "type":"string"}"#,
    printcolumn = r#"{"name":"Acceptance Test", "jsonPath":".spec.acceptanceTest", "type":"boolean"}"#,
    printcolumn = r#"{"name":"Cleanup After", "jsonPath":".spec.cleanupAfter", "type":"string"}"#,
    printcolumn = r#"{"name":"Age", "jsonPath":".metadata.creationTimestamp", "type":"date"}"#
)]
pub struct JobTemplateSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none", rename = "acceptanceTest")]
    pub acceptance_test: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none", rename = "cleanupAfter")]
    pub cleanup_after: Option<String>,

    pub spec: serde_json::Value,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct JobTemplateStatus {}

impl JobTemplate {
    pub fn is_cronjob(&self) -> bool {
        self.spec.schedule.is_some()
    }

    pub fn is_acceptance_test(&self) -> bool {
        self.spec.acceptance_test.unwrap_or(false)
    }

    pub fn cleanup_after(&self) -> &str {
        self.spec.cleanup_after.as_deref().unwrap_or("30m")
    }
}
