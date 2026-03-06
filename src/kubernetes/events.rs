use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Event;
use kube::api::ListParams;
use kube::{Api, Client, ResourceExt};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventInfo {
    pub type_: String,
    pub reason: String,
    pub message: String,
    pub timestamp: String,
    pub source: String,
    pub count: Option<i32>,
}

pub async fn fetch_job_events(client: &Client, namespace: &str, job: &Job) -> Vec<EventInfo> {
    let event_api: Api<Event> = Api::namespaced(client.clone(), namespace);
    let job_name = job.name_any();
    let job_uid = job.uid().unwrap_or_default();

    // Fetch events for the Job itself
    let job_events = event_api
        .list(&ListParams::default().fields(&format!(
            "involvedObject.kind=Job,involvedObject.name={},involvedObject.uid={}",
            job_name, job_uid
        )))
        .await;

    // Fetch events for the Job's pods
    let pod_api: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client.clone(), namespace);
    let pod_names: Vec<String> = pod_api
        .list(&ListParams::default().labels(&format!("job-name={}", job_name)))
        .await
        .map(|list| list.items.iter().map(|p| p.name_any()).collect())
        .unwrap_or_default();

    let mut all_events = Vec::new();

    if let Ok(list) = job_events {
        for event in list.items {
            all_events.push(event_to_info(&event));
        }
    }

    for pod_name in &pod_names {
        if let Ok(list) = event_api
            .list(&ListParams::default().fields(&format!(
                "involvedObject.kind=Pod,involvedObject.name={}",
                pod_name
            )))
            .await
        {
            for event in list.items {
                all_events.push(event_to_info(&event));
            }
        }
    }

    // Sort by timestamp ascending
    all_events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    all_events
}

fn event_to_info(event: &Event) -> EventInfo {
    let timestamp = event
        .last_timestamp
        .as_ref()
        .map(|t| t.0.format("%Y-%m-%d %H:%M:%S").to_string())
        .or_else(|| {
            event
                .event_time
                .as_ref()
                .map(|t| t.0.format("%Y-%m-%d %H:%M:%S").to_string())
        })
        .unwrap_or_default();

    let source = event
        .source
        .as_ref()
        .and_then(|s| s.component.as_deref())
        .unwrap_or("unknown")
        .to_string();

    EventInfo {
        type_: event.type_.clone().unwrap_or_default(),
        reason: event.reason.clone().unwrap_or_default(),
        message: event.message.clone().unwrap_or_default(),
        timestamp,
        source,
        count: event.count,
    }
}
