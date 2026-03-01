use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::service::{AppService, ServiceError};
use crate::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, NotePatch, ProjectPatch,
    SearchFilters, TaskFilters, TaskPatch,
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
    let mut reader = BufReader::new(stdin);
    let mut writer = BufWriter::new(stdout);

    while let Some((req, framing)) = read_next_request(&mut reader).await? {
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

            write_response(&mut writer, framing, &payload).await?;
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
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "subscribe": false, "listChanged": false }
            }
        })),
        "ping" => Ok(json!({})),
        "resources/list" => Ok(json!({ "resources": [] })),
        "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
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
            let result = handle_tool_call(service, name, arguments).await?;
            Ok(tool_call_result(result))
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
        "list_projects" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
            let include_archived = args
                .get("include_archived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let rows = service
                .list_projects(limit, include_archived)
                .map_err(map_anyhow_to_rpc)?;
            json!(rows)
        }
        "get_project_workspace" => {
            let project_id = args
                .get("project_id")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "get_project_workspace missing project_id", None))?;
            let workspace = service
                .load_project_workspace(project_id)
                .map_err(map_anyhow_to_rpc)?;
            json!(workspace)
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
        "update_project" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "update_project missing id", None))?;
            let expected_revision = args
                .get("expected_revision")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "update_project missing expected_revision", None))?;
            let patch: ProjectPatch = serde_json::from_value(
                args.get("patch")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default())),
            )
            .map_err(|e| {
                rpc_err(
                    -32602,
                    "invalid update_project patch",
                    Some(json!({"error": e.to_string()})),
                )
            })?;
            let entity = service
                .update_project(id, patch, expected_revision, agent.clone())
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
        "reorder_project_tasks" => {
            let project_id = args
                .get("project_id")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "reorder_project_tasks missing project_id", None))?;
            let ordered_active_task_ids: Vec<String> =
                serde_json::from_value(args.get("ordered_active_task_ids").cloned().ok_or_else(
                    || {
                        rpc_err(
                            -32602,
                            "reorder_project_tasks missing ordered_active_task_ids",
                            None,
                        )
                    },
                )?)
                .map_err(|e| {
                    rpc_err(
                        -32602,
                        "invalid ordered_active_task_ids",
                        Some(json!({"error": e.to_string()})),
                    )
                })?;
            let workspace = service
                .reorder_project_tasks(project_id, &ordered_active_task_ids, agent.clone())
                .map_err(map_service_to_rpc)?;
            json!(workspace)
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

#[derive(Debug, Clone, Copy)]
enum MessageFraming {
    JsonLine,
    ContentLength,
}

async fn read_next_request(
    reader: &mut BufReader<tokio::io::Stdin>,
) -> Result<Option<(RpcRequest, MessageFraming)>> {
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(None);
        }

        if line.trim().is_empty() {
            continue;
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);

        if trimmed.starts_with('{') {
            let req: RpcRequest = match serde_json::from_str(trimmed) {
                Ok(req) => req,
                Err(err) => {
                    warn!(error = %err, "invalid stdio JSON request");
                    continue;
                }
            };
            return Ok(Some((req, MessageFraming::JsonLine)));
        }

        let Some(content_length) = parse_content_length(trimmed) else {
            warn!(line = trimmed, "invalid mcp stdio prelude");
            continue;
        };

        loop {
            line.clear();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                warn!("unexpected EOF while reading mcp headers");
                return Ok(None);
            }
            if line.trim().is_empty() {
                break;
            }
        }

        let mut body = vec![0u8; content_length];
        if let Err(err) = reader.read_exact(&mut body).await {
            warn!(error = %err, "failed reading framed mcp request body");
            return Ok(None);
        }

        let req: RpcRequest = match serde_json::from_slice(&body) {
            Ok(req) => req,
            Err(err) => {
                warn!(error = %err, "invalid framed mcp JSON request");
                continue;
            }
        };

        return Ok(Some((req, MessageFraming::ContentLength)));
    }
}

fn parse_content_length(line: &str) -> Option<usize> {
    let (name, value) = line.split_once(':')?;
    if !name.trim().eq_ignore_ascii_case("content-length") {
        return None;
    }
    value.trim().parse().ok()
}

async fn write_response(
    writer: &mut BufWriter<tokio::io::Stdout>,
    framing: MessageFraming,
    payload: &Value,
) -> Result<()> {
    let body = payload.to_string();

    match framing {
        MessageFraming::JsonLine => {
            writer.write_all(body.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        MessageFraming::ContentLength => {
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            writer.write_all(header.as_bytes()).await?;
            writer.write_all(body.as_bytes()).await?;
        }
    }

    writer.flush().await?;
    Ok(())
}

fn tool_call_result(result: Value) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": result.to_string()
            }
        ],
        "structuredContent": result
    })
}

fn is_direct_tool_method(method: &str) -> bool {
    matches!(
        method,
        "search_context"
            | "read_entity"
            | "list_tasks"
            | "list_projects"
            | "get_project_workspace"
            | "create_project"
            | "update_project"
            | "create_task"
            | "update_task"
            | "reorder_project_tasks"
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
            search_context_input_schema(),
        ),
        tool_def(
            "read_entity",
            "Read entity by ID or path",
            read_entity_input_schema(),
        ),
        tool_def(
            "list_tasks",
            "List tasks by status/priority/project/assignee filters",
            list_tasks_input_schema(),
        ),
        tool_def(
            "list_projects",
            "List projects by archived state",
            list_projects_input_schema(),
        ),
        tool_def(
            "get_project_workspace",
            "Load a project workspace with tasks and notes",
            get_project_workspace_input_schema(),
        ),
        tool_def(
            "create_project",
            "Create a project markdown entity",
            create_project_input_schema(),
        ),
        tool_def(
            "update_project",
            "Update a project with optimistic revision lock",
            update_project_input_schema(),
        ),
        tool_def(
            "create_task",
            "Create a task markdown entity",
            create_task_input_schema(),
        ),
        tool_def(
            "update_task",
            "Update a task with optimistic revision lock",
            update_task_input_schema(),
        ),
        tool_def(
            "reorder_project_tasks",
            "Reorder active project tasks and return the workspace snapshot",
            reorder_project_tasks_input_schema(),
        ),
        tool_def(
            "create_note",
            "Create a note markdown entity",
            create_note_input_schema(),
        ),
        tool_def(
            "update_note",
            "Update a note with optimistic revision lock",
            update_note_input_schema(),
        ),
        tool_def(
            "archive_entity",
            "Archive an entity with optimistic revision lock",
            archive_entity_input_schema(),
        ),
        tool_def(
            "list_recent_activity",
            "List recent immutable activity events",
            list_recent_activity_input_schema(),
        ),
    ]
}

fn tool_def(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    })
}

fn search_context_input_schema() -> Value {
    object_schema(
        json!({
            "query": { "type": "string" },
            "filters": search_filters_schema(),
            "limit": { "type": "integer", "minimum": 0 }
        }),
        &["query"],
    )
}

fn read_entity_input_schema() -> Value {
    object_schema(
        json!({
            "id_or_path": { "type": "string" }
        }),
        &["id_or_path"],
    )
}

fn list_tasks_input_schema() -> Value {
    object_schema(
        json!({
            "filters": task_filters_schema()
        }),
        &[],
    )
}

fn list_projects_input_schema() -> Value {
    object_schema(
        json!({
            "limit": { "type": "integer", "minimum": 0 },
            "include_archived": { "type": "boolean" }
        }),
        &[],
    )
}

fn get_project_workspace_input_schema() -> Value {
    object_schema(
        json!({
            "project_id": { "type": "string" }
        }),
        &["project_id"],
    )
}

fn create_project_input_schema() -> Value {
    object_schema(
        json!({
            "title": { "type": "string" },
            "owner": { "type": "string" },
            "source_kind": project_source_kind_schema(),
            "source_locator": { "type": "string" },
            "tags": string_array_schema(),
            "body": { "type": "string" }
        }),
        &["title"],
    )
}

fn update_project_input_schema() -> Value {
    object_schema(
        json!({
            "id": { "type": "string" },
            "expected_revision": { "type": "string" },
            "patch": project_patch_schema()
        }),
        &["id", "expected_revision"],
    )
}

fn create_task_input_schema() -> Value {
    object_schema(
        json!({
            "title": { "type": "string" },
            "project_id": { "type": "string" },
            "status": task_status_schema(),
            "priority": task_priority_schema(),
            "assignee": { "type": "string" },
            "due_at": date_time_string_schema(),
            "sort_order": { "type": "integer" },
            "sync_kind": task_sync_kind_schema(),
            "sync_path": { "type": "string" },
            "sync_key": { "type": "string" },
            "sync_managed": { "type": "boolean" },
            "tags": string_array_schema(),
            "body": { "type": "string" }
        }),
        &["title", "project_id"],
    )
}

fn update_task_input_schema() -> Value {
    object_schema(
        json!({
            "id": { "type": "string" },
            "expected_revision": { "type": "string" },
            "patch": task_patch_schema()
        }),
        &["id", "expected_revision"],
    )
}

fn reorder_project_tasks_input_schema() -> Value {
    object_schema(
        json!({
            "project_id": { "type": "string" },
            "ordered_active_task_ids": {
                "type": "array",
                "items": { "type": "string" }
            }
        }),
        &["project_id", "ordered_active_task_ids"],
    )
}

fn create_note_input_schema() -> Value {
    object_schema(
        json!({
            "title": { "type": "string" },
            "project_id": { "type": "string" },
            "tags": string_array_schema(),
            "body": { "type": "string" }
        }),
        &["title"],
    )
}

fn update_note_input_schema() -> Value {
    object_schema(
        json!({
            "id": { "type": "string" },
            "expected_revision": { "type": "string" },
            "patch": note_patch_schema()
        }),
        &["id", "expected_revision"],
    )
}

fn archive_entity_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "id": { "type": "string" },
            "entity_id": { "type": "string" },
            "expected_revision": { "type": "string" }
        },
        "required": ["expected_revision"],
        "anyOf": [
            { "required": ["id"] },
            { "required": ["entity_id"] }
        ]
    })
}

fn list_recent_activity_input_schema() -> Value {
    object_schema(
        json!({
            "since": date_time_string_schema(),
            "limit": { "type": "integer", "minimum": 0 }
        }),
        &[],
    )
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required
    })
}

fn search_filters_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "entity_type": entity_type_schema(),
            "project_id": { "type": "string" },
            "include_archived": { "type": "boolean" }
        }
    })
}

fn task_filters_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "status": task_status_schema(),
            "priority": task_priority_schema(),
            "project_id": { "type": "string" },
            "assignee": { "type": "string" },
            "include_archived": { "type": "boolean" },
            "limit": { "type": "integer", "minimum": 0 }
        }
    })
}

fn project_patch_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "title": { "type": "string" },
            "status": project_status_schema(),
            "owner": { "type": ["string", "null"] },
            "source_kind": {
                "type": ["string", "null"],
                "enum": ["local", "github", null]
            },
            "source_locator": { "type": ["string", "null"] },
            "sync_source_key": { "type": ["string", "null"] },
            "last_synced_at": {
                "type": ["string", "null"],
                "format": "date-time"
            },
            "last_sync_summary": { "type": ["string", "null"] },
            "tags": string_array_schema(),
            "body": { "type": "string" }
        }
    })
}

fn task_patch_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "title": { "type": "string" },
            "status": task_status_schema(),
            "priority": task_priority_schema(),
            "assignee": { "type": "string" },
            "due_at": { "type": "string" },
            "sort_order": { "type": "integer" },
            "tags": string_array_schema(),
            "body": { "type": "string" }
        }
    })
}

fn note_patch_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "title": { "type": "string" },
            "project_id": { "type": "string" },
            "tags": string_array_schema(),
            "body": { "type": "string" }
        }
    })
}

fn entity_type_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["task", "project", "note"]
    })
}

fn task_status_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["backlog", "todo", "in_progress", "blocked", "done"]
    })
}

fn task_priority_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["P0", "P1", "P2", "P3"]
    })
}

fn project_status_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["active", "paused", "archived"]
    })
}

fn project_source_kind_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["local", "github"]
    })
}

fn task_sync_kind_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["repo_markdown"]
    })
}

fn string_array_schema() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" }
    })
}

fn date_time_string_schema() -> Value {
    json!({
        "type": "string",
        "format": "date-time"
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
