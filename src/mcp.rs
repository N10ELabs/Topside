use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::service::{AppService, ServiceError};
use crate::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, NotePatch, SearchFilters,
    TaskFilters, TaskPatch,
};

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default, rename = "jsonrpc")]
    pub _jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

pub fn spawn_stdio_server(service: Arc<AppService>) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run_stdio_server_forever(service).await {
            error!(error = %err, "mcp stdio server terminated with error");
        }
    })
}

pub async fn run_stdio_server_forever(service: Arc<AppService>) -> Result<()> {
    info!("starting stdio mcp server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let mut writer = BufWriter::new(stdout);

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let req: RpcRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                warn!(error = %err, "invalid stdio JSON request");
                continue;
            }
        };

        let id = req.id.clone();
        let response = handle_request(service.clone(), req).await;

        if let Some(id) = id {
            let payload = match response {
                Ok(result) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }),
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": err,
                }),
            };

            writer.write_all(payload.to_string().as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
    }

    Ok(())
}

async fn handle_request(
    service: Arc<AppService>,
    req: RpcRequest,
) -> std::result::Result<Value, Value> {
    let method = req.method.as_str();

    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": {"name": "n10e", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"tools": {}}
        })),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = req
                .params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "missing tools/call name", None))?;
            let arguments = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            handle_tool_call(service, name, arguments).await
        }
        other => {
            if is_direct_tool_method(other) {
                handle_tool_call(service, other, req.params).await
            } else {
                Err(rpc_err(
                    -32601,
                    "method not found",
                    Some(json!({ "method": other })),
                ))
            }
        }
    }
}

async fn handle_tool_call(
    service: Arc<AppService>,
    name: &str,
    args: Value,
) -> std::result::Result<Value, Value> {
    let agent = Actor::agent("mcp");

    let result = match name {
        "search_context" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "search_context missing query", None))?;
            let filters = args
                .get("filters")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| {
                    rpc_err(
                        -32602,
                        "invalid search filters",
                        Some(json!({"error": e.to_string()})),
                    )
                })?
                .unwrap_or(SearchFilters {
                    entity_type: None,
                    project_id: None,
                    include_archived: false,
                });
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|v| v as usize);

            let rows = service
                .search_context(query, &filters, limit)
                .map_err(map_anyhow_to_rpc)?;
            json!(rows)
        }
        "read_entity" => {
            let id_or_path = args
                .get("id_or_path")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "read_entity missing id_or_path", None))?;
            let entity = service.read_entity(id_or_path).map_err(map_anyhow_to_rpc)?;
            json!(entity)
        }
        "list_tasks" => {
            let filters = args
                .get("filters")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| {
                    rpc_err(
                        -32602,
                        "invalid task filters",
                        Some(json!({"error": e.to_string()})),
                    )
                })?
                .unwrap_or(TaskFilters {
                    status: None,
                    priority: None,
                    project_id: None,
                    assignee: None,
                    include_archived: false,
                    limit: Some(100),
                });
            let rows = service.list_tasks(&filters).map_err(map_anyhow_to_rpc)?;
            json!(rows)
        }
        "create_project" => {
            let payload: CreateProjectPayload = serde_json::from_value(args).map_err(|e| {
                rpc_err(
                    -32602,
                    "invalid create_project payload",
                    Some(json!({"error": e.to_string()})),
                )
            })?;
            let entity = service
                .create_project(payload, agent.clone())
                .map_err(map_service_to_rpc)?;
            json!(entity)
        }
        "create_task" => {
            let payload: CreateTaskPayload = serde_json::from_value(args).map_err(|e| {
                rpc_err(
                    -32602,
                    "invalid create_task payload",
                    Some(json!({"error": e.to_string()})),
                )
            })?;
            let entity = service
                .create_task(payload, agent.clone())
                .map_err(map_service_to_rpc)?;
            json!(entity)
        }
        "update_task" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "update_task missing id", None))?;
            let expected_revision = args
                .get("expected_revision")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "update_task missing expected_revision", None))?;
            let patch: TaskPatch = serde_json::from_value(
                args.get("patch")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default())),
            )
            .map_err(|e| {
                rpc_err(
                    -32602,
                    "invalid update_task patch",
                    Some(json!({"error": e.to_string()})),
                )
            })?;
            let entity = service
                .update_task(id, patch, expected_revision, agent.clone())
                .map_err(map_service_to_rpc)?;
            json!(entity)
        }
        "create_note" => {
            let payload: CreateNotePayload = serde_json::from_value(args).map_err(|e| {
                rpc_err(
                    -32602,
                    "invalid create_note payload",
                    Some(json!({"error": e.to_string()})),
                )
            })?;
            let entity = service
                .create_note(payload, agent.clone())
                .map_err(map_service_to_rpc)?;
            json!(entity)
        }
        "update_note" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "update_note missing id", None))?;
            let expected_revision = args
                .get("expected_revision")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "update_note missing expected_revision", None))?;
            let patch: NotePatch = serde_json::from_value(
                args.get("patch")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default())),
            )
            .map_err(|e| {
                rpc_err(
                    -32602,
                    "invalid update_note patch",
                    Some(json!({"error": e.to_string()})),
                )
            })?;
            let entity = service
                .update_note(id, patch, expected_revision, agent.clone())
                .map_err(map_service_to_rpc)?;
            json!(entity)
        }
        "archive_entity" => {
            let id = args
                .get("id")
                .or_else(|| args.get("entity_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "archive_entity missing id", None))?;
            let expected_revision = args
                .get("expected_revision")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "archive_entity missing expected_revision", None))?;
            let entity = service
                .archive_entity(id, expected_revision, agent.clone())
                .map_err(map_service_to_rpc)?;
            json!(entity)
        }
        "list_recent_activity" => {
            let since = args
                .get("since")
                .and_then(Value::as_str)
                .map(|v| DateTime::parse_from_rfc3339(v).map(|dt| dt.with_timezone(&Utc)))
                .transpose()
                .map_err(|e| {
                    rpc_err(
                        -32602,
                        "invalid since timestamp",
                        Some(json!({"error": e.to_string()})),
                    )
                })?;
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
            let activity = service
                .list_recent_activity(since, limit)
                .map_err(map_anyhow_to_rpc)?;
            json!(activity)
        }
        _ => {
            return Err(rpc_err(
                -32601,
                "unknown tool name",
                Some(json!({"tool": name})),
            ));
        }
    };

    Ok(result)
}

fn is_direct_tool_method(method: &str) -> bool {
    matches!(
        method,
        "search_context"
            | "read_entity"
            | "list_tasks"
            | "create_project"
            | "create_task"
            | "update_task"
            | "create_note"
            | "update_note"
            | "archive_entity"
            | "list_recent_activity"
    )
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool_def(
            "search_context",
            "Search indexed context across tasks/projects/notes",
        ),
        tool_def("read_entity", "Read entity by ID or path"),
        tool_def(
            "list_tasks",
            "List tasks by status/priority/project/assignee filters",
        ),
        tool_def("create_project", "Create a project markdown entity"),
        tool_def("create_task", "Create a task markdown entity"),
        tool_def("update_task", "Update a task with optimistic revision lock"),
        tool_def("create_note", "Create a note markdown entity"),
        tool_def("update_note", "Update a note with optimistic revision lock"),
        tool_def(
            "archive_entity",
            "Archive an entity with optimistic revision lock",
        ),
        tool_def(
            "list_recent_activity",
            "List recent immutable activity events",
        ),
    ]
}

fn tool_def(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "additionalProperties": true
        }
    })
}

fn map_anyhow_to_rpc(err: anyhow::Error) -> Value {
    rpc_err(
        -32000,
        "internal server error",
        Some(json!({"error": err.to_string()})),
    )
}

fn map_service_to_rpc(err: ServiceError) -> Value {
    match err {
        ServiceError::Conflict { expected, current } => rpc_err(
            -32010,
            "revision conflict",
            Some(json!({"expected_revision": expected, "current_revision": current})),
        ),
        ServiceError::Other(err) => rpc_err(
            -32000,
            "internal server error",
            Some(json!({"error": err.to_string()})),
        ),
    }
}

fn rpc_err(code: i32, message: &str, data: Option<Value>) -> Value {
    match data {
        Some(data) => json!({ "code": code, "message": message, "data": data }),
        None => json!({ "code": code, "message": message }),
    }
}
