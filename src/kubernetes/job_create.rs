use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::ResourceExt;

use crate::kubernetes::JobTemplate;

pub fn build_job(
    job_template: &JobTemplate,
    args: Option<Vec<String>>,
    env: Option<Vec<(String, String)>>,
) -> Result<Job, String> {
    let namespace = job_template
        .metadata
        .namespace
        .as_deref()
        .unwrap_or("default");
    let name = job_template.metadata.name.as_deref().unwrap_or("unknown");

    let mut pod_spec: serde_json::Value = job_template.spec.spec.clone();

    // Inject emptyDir volume for job output
    if pod_spec.get("volumes").is_none() {
        pod_spec["volumes"] = serde_json::json!([]);
    }
    if let Some(volumes) = pod_spec["volumes"].as_array_mut() {
        volumes.push(serde_json::json!({"name": "job-output", "emptyDir": {}}));
    }

    // Modify the first container: add volume mount, optionally override args and env
    if let Some(containers) = pod_spec.get_mut("containers").and_then(|c| c.as_array_mut()) {
        if let Some(container) = containers.first_mut() {
            // Add /job-output volume mount
            if container.get("volumeMounts").is_none() {
                container["volumeMounts"] = serde_json::json!([]);
            }
            if let Some(mounts) = container["volumeMounts"].as_array_mut() {
                mounts.push(
                    serde_json::json!({"name": "job-output", "mountPath": "/job-output"}),
                );
            }

            // Override args if provided
            if let Some(ref override_args) = args {
                if !override_args.is_empty()
                    || job_template
                        .spec
                        .spec
                        .get("containers")
                        .and_then(|c| c.as_array())
                        .and_then(|a| a.first())
                        .and_then(|c| c.get("args"))
                        .is_some()
                {
                    container["args"] =
                        serde_json::to_value(override_args).unwrap_or_default();
                }
            }

            // Override env if provided
            if let Some(ref override_env) = env {
                let original_env = container
                    .get("env")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default();

                let mut new_env: Vec<serde_json::Value> = Vec::new();

                // Preserve all valueFrom entries as-is
                for entry in &original_env {
                    if entry.get("valueFrom").is_some() {
                        new_env.push(entry.clone());
                    }
                }

                // Add the editable env vars
                for (name, value) in override_env {
                    new_env.push(serde_json::json!({
                        "name": name,
                        "value": value,
                    }));
                }

                container["env"] = serde_json::to_value(&new_env).unwrap_or_default();
            }
        }
    }

    // Inject job-output sidecar container
    let jobfather_url = std::env::var("JOBFATHER_URL")
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    if !jobfather_url.is_empty() {
        if pod_spec.get("initContainers").is_none() {
            pod_spec["initContainers"] = serde_json::json!([]);
        }
        if let Some(init_containers) = pod_spec["initContainers"].as_array_mut() {
            init_containers.push(serde_json::json!({
                "name": "job-output-sidecar",
                "image": "curlimages/curl:latest",
                "restartPolicy": "Always",
                "command": ["sh", "-c", concat!(
                    "upload() { ",
                    "for f in result.json report.md test-results.xml archive.tar.gz; do ",
                    "[ -f \"/job-output/$f\" ] && curl -sf -X PUT --data-binary \"@/job-output/$f\" ",
                    "\"$JOBFATHER_URL/api/jobs/$JOBFATHER_NAMESPACE/$JOBFATHER_JOB_NAME/output/$f\" || true; ",
                    "done; ",
                    "if [ -d /job-output/test-snapshots ] && [ \"$(ls -A /job-output/test-snapshots 2>/dev/null)\" ]; then ",
                    "tar czf /tmp/test-snapshots.tar.gz -C /job-output/test-snapshots . && ",
                    "curl -sf -X PUT --data-binary @/tmp/test-snapshots.tar.gz ",
                    "\"$JOBFATHER_URL/api/jobs/$JOBFATHER_NAMESPACE/$JOBFATHER_JOB_NAME/output/test-snapshots.tar.gz\" || true; ",
                    "fi; ",
                    "exit 0; }; ",
                    "trap upload TERM; ",
                    "while true; do sleep 3600 & wait; done"
                )],
                "env": [
                    {"name": "JOBFATHER_URL", "value": &jobfather_url},
                    {"name": "JOBFATHER_NAMESPACE", "value": namespace},
                    {"name": "JOBFATHER_JOB_NAME", "valueFrom": {"fieldRef": {"fieldPath": "metadata.labels['job-name']"}}}
                ],
                "volumeMounts": [{"name": "job-output", "mountPath": "/job-output"}]
            }));
        }
    }

    // Add restartPolicy if not set
    if pod_spec.get("restartPolicy").is_none() {
        pod_spec["restartPolicy"] = serde_json::Value::String("Never".to_string());
    }

    let pod_template: k8s_openapi::api::core::v1::PodSpec =
        serde_json::from_value(pod_spec).map_err(|e| format!("Invalid pod spec: {}", e))?;

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let job_name = format!("{}-{}", name, timestamp);

    // Inherit artifact-sha and config-sha annotations from the JobTemplate
    let sha_annotations: std::collections::BTreeMap<String, String> = job_template
        .metadata
        .annotations
        .as_ref()
        .map(|a| {
            a.iter()
                .filter(|(k, _)| *k == "artifactSha" || *k == "configSha")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();

    let job = Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(namespace.to_string()),
            annotations: if sha_annotations.is_empty() {
                None
            } else {
                Some(sha_annotations)
            },
            owner_references: Some(vec![
                k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                    api_version: "jobfather.coolkev.com/v1".to_string(),
                    kind: "JobTemplate".to_string(),
                    name: name.to_string(),
                    uid: job_template.uid().unwrap_or_default(),
                    controller: Some(true),
                    block_owner_deletion: Some(true),
                },
            ]),
            ..Default::default()
        },
        spec: Some(JobSpec {
            template: PodTemplateSpec {
                spec: Some(pod_template),
                ..Default::default()
            },
            backoff_limit: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    };

    Ok(job)
}
