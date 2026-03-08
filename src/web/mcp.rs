use actix_web::{get, post, web, HttpResponse, Responder};
use kube::Client;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde_json::{json, Value};

/// MCP Streamable HTTP endpoint — handles JSON-RPC 2.0 requests.
#[post("/mcp")]
pub async fn mcp_post(
    body: web::Json<Value>,
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let request = body.into_inner();
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("");

    let response = match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "jobfather",
                    "version": "0.1.0"
                }
            }
        }),
        "notifications/initialized" => {
            return HttpResponse::NoContent().finish();
        }
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": tool_definitions()
            }
        }),
        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or(json!({}));
            let tool_name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(json!({}));
            let result = call_tool(tool_name, &arguments, &client, &pool).await;
            match result {
                Ok(text) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": text}]
                    }
                }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": e}],
                        "isError": true
                    }
                }),
            }
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {}", method)
            }
        }),
    };

    HttpResponse::Ok()
        .content_type("application/json")
        .json(response)
}

/// MCP endpoint discovery (GET returns server capabilities).
#[get("/mcp")]
pub async fn mcp_get() -> impl Responder {
    HttpResponse::Ok()
        .content_type("application/json")
        .json(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "jobfather",
                "version": "0.1.0"
            }
        }))
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "list_job_templates",
            "description": "List all JobTemplates across all namespaces",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        json!({
            "name": "get_job_template",
            "description": "Get details of a specific JobTemplate",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string", "description": "Kubernetes namespace"},
                    "name": {"type": "string", "description": "JobTemplate name"}
                },
                "required": ["namespace", "name"]
            }
        }),
        json!({
            "name": "list_jobs",
            "description": "List jobs for a JobTemplate (newest first, includes both live and archived)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string", "description": "Kubernetes namespace"},
                    "name": {"type": "string", "description": "JobTemplate name"}
                },
                "required": ["namespace", "name"]
            }
        }),
        json!({
            "name": "get_job",
            "description": "Get details of a specific job",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string", "description": "Kubernetes namespace"},
                    "name": {"type": "string", "description": "Job name"}
                },
                "required": ["namespace", "name"]
            }
        }),
        json!({
            "name": "get_job_logs",
            "description": "Get container logs for a job",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string", "description": "Kubernetes namespace"},
                    "name": {"type": "string", "description": "Job name"},
                    "tail_lines": {"type": "integer", "description": "Number of lines from the end to return (optional)"}
                },
                "required": ["namespace", "name"]
            }
        }),
        json!({
            "name": "get_job_events",
            "description": "Get Kubernetes events for a job",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string", "description": "Kubernetes namespace"},
                    "name": {"type": "string", "description": "Job name"}
                },
                "required": ["namespace", "name"]
            }
        }),
        json!({
            "name": "get_job_test_results",
            "description": "Get parsed JUnit test results for a job",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string", "description": "Kubernetes namespace"},
                    "name": {"type": "string", "description": "Job name"}
                },
                "required": ["namespace", "name"]
            }
        }),
        json!({
            "name": "get_job_output",
            "description": "Get job output file content (result.json or report.md)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string", "description": "Kubernetes namespace"},
                    "name": {"type": "string", "description": "Job name"},
                    "filename": {"type": "string", "description": "Output filename: 'result.json' or 'report.md'", "enum": ["result.json", "report.md"]}
                },
                "required": ["namespace", "name", "filename"]
            }
        }),
        json!({
            "name": "get_snapshot_diff",
            "description": "Get snapshot comparison diff for a job",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string", "description": "Kubernetes namespace"},
                    "name": {"type": "string", "description": "Job name"}
                },
                "required": ["namespace", "name"]
            }
        }),
        json!({
            "name": "run_job",
            "description": "Trigger a new job from a JobTemplate",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string", "description": "Kubernetes namespace"},
                    "name": {"type": "string", "description": "JobTemplate name"},
                    "args": {"type": "array", "items": {"type": "string"}, "description": "Command arguments (optional)"},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Environment variables (optional)"}
                },
                "required": ["namespace", "name"]
            }
        }),
    ]
}

async fn call_tool(
    name: &str,
    args: &Value,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
) -> Result<String, String> {
    match name {
        "list_job_templates" => {
            let api: kube::Api<crate::kubernetes::JobTemplate> =
                kube::Api::all(client.clone());
            match api.list(&Default::default()).await {
                Ok(list) => {
                    let templates: Vec<_> = list.items.iter().map(super::api::jt_to_api).collect();
                    serde_json::to_string_pretty(&templates)
                        .map_err(|e| format!("Serialization error: {}", e))
                }
                Err(e) => Err(format!("Failed to list JobTemplates: {}", e)),
            }
        }
        "get_job_template" => {
            let (ns, n) = get_ns_name(args)?;
            let api: kube::Api<crate::kubernetes::JobTemplate> =
                kube::Api::namespaced(client.clone(), &ns);
            match api.get(&n).await {
                Ok(jt) => serde_json::to_string_pretty(&super::api::jt_to_api(&jt))
                    .map_err(|e| format!("Serialization error: {}", e)),
                Err(e) => Err(format!("JobTemplate not found: {}", e)),
            }
        }
        "list_jobs" => {
            let (ns, n) = get_ns_name(args)?;
            let jobs = super::api::list_jobs_for_template_inner(&ns, &n, client, pool).await;
            serde_json::to_string_pretty(&jobs)
                .map_err(|e| format!("Serialization error: {}", e))
        }
        "get_job" => {
            let (ns, n) = get_ns_name(args)?;
            match super::api::get_job_inner(&ns, &n, client, pool).await {
                Some(job) => serde_json::to_string_pretty(&job)
                    .map_err(|e| format!("Serialization error: {}", e)),
                None => Err(format!("Job {}/{} not found", ns, n)),
            }
        }
        "get_job_logs" => {
            let (ns, n) = get_ns_name(args)?;
            let tail = args.get("tail_lines").and_then(|t| t.as_i64());
            super::api::get_job_logs_inner(&ns, &n, tail, client, pool).await
        }
        "get_job_events" => {
            let (ns, n) = get_ns_name(args)?;
            let events = super::api::get_job_events_inner(&ns, &n, client, pool).await;
            serde_json::to_string_pretty(&events)
                .map_err(|e| format!("Serialization error: {}", e))
        }
        "get_job_test_results" => {
            let (ns, n) = get_ns_name(args)?;
            match super::api::get_test_results_inner(&ns, &n, pool) {
                Some(results) => serde_json::to_string_pretty(&results)
                    .map_err(|e| format!("Serialization error: {}", e)),
                None => Err("No test results found".to_string()),
            }
        }
        "get_job_output" => {
            let (ns, n) = get_ns_name(args)?;
            let filename = args
                .get("filename")
                .and_then(|f| f.as_str())
                .ok_or("Missing 'filename' parameter")?;
            super::api::get_job_output_inner(&ns, &n, filename, pool)
        }
        "get_snapshot_diff" => {
            let (ns, n) = get_ns_name(args)?;
            match super::api::get_snapshot_diff_inner(&ns, &n, pool) {
                Some(diff) => serde_json::to_string_pretty(&diff)
                    .map_err(|e| format!("Serialization error: {}", e)),
                None => Err("No snapshot diff found".to_string()),
            }
        }
        "run_job" => {
            let (ns, n) = get_ns_name(args)?;
            let run_args = args.get("args").and_then(|a| {
                serde_json::from_value::<Vec<String>>(a.clone()).ok()
            });
            let run_env = args.get("env").and_then(|e| {
                serde_json::from_value::<std::collections::HashMap<String, String>>(e.clone()).ok()
            });
            super::api::run_job_inner(&ns, &n, run_args, run_env, client).await
        }
        _ => Err(format!("Unknown tool: {}", name)),
    }
}

fn get_ns_name(args: &Value) -> Result<(String, String), String> {
    let ns = args
        .get("namespace")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'namespace' parameter")?
        .to_string();
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'name' parameter")?
        .to_string();
    Ok((ns, name))
}
