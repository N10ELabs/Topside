use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config::PollConfig;
use crate::markdown::render_markdown_html;
use crate::service::{AppService, ServiceError};
use crate::types::{
    Actor, CreateNotePayload, CreateProjectPayload, NotePatch, ProjectItem, ProjectSourceKind,
    ProjectWorkspace, TaskPatch, TaskStatus,
};

#[derive(Clone)]
pub struct WebState {
    pub service: Arc<AppService>,
    pub poll: PollConfig,
}

pub fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/reindex", post(reindex_now))
        .route("/api/projects", get(api_projects).post(api_create_project))
        .route("/api/projects/{id}/workspace", get(api_project_workspace))
        .route("/api/tasks", post(api_create_task))
        .route("/api/tasks/{id}", patch(api_update_task))
        .route("/api/tasks/reorder", post(api_reorder_tasks))
        .route("/api/notes", post(api_create_note))
        .route("/api/notes/{id}", patch(api_update_note))
        .route("/api/system/pick-directory", post(api_pick_directory))
        .with_state(state)
}

#[derive(Template)]
#[template(path = "index.html")]
struct DashboardTemplate {
    initial_state_json: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProjectQuery {
    project_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
    expected_revision: Option<String>,
    current_revision: Option<String>,
}

#[derive(Debug, Serialize)]
struct UiStatePayload {
    projects: Vec<ProjectSummary>,
    selected_project_id: Option<String>,
    workspace: Option<WorkspacePayload>,
    server_port: u16,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectSummary {
    id: String,
    title: String,
    source_kind: Option<String>,
    source_locator: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorkspacePayload {
    project: ProjectSummary,
    active_tasks: Vec<TaskPayload>,
    done_tasks: Vec<TaskPayload>,
    notes: Vec<NotePayload>,
    suggested_open_note_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct TaskPayload {
    id: String,
    title: String,
    revision: String,
    completed_at_label: Option<String>,
    updated_at_label: String,
    sort_order: i64,
}

#[derive(Debug, Serialize)]
struct NotePayload {
    id: String,
    title: String,
    body: String,
    rendered_html: String,
    revision: String,
    updated_at_label: String,
}

#[derive(Debug, Serialize)]
struct TaskMutationResponse {
    workspace: WorkspacePayload,
    created_task_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct NoteMutationResponse {
    workspace: WorkspacePayload,
    created_note_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateProjectRequest {
    mode: String,
    title: Option<String>,
    source_locator: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    project_id: String,
    #[serde(default)]
    title: String,
    after_task_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateTaskRequest {
    expected_revision: String,
    title: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReorderTasksRequest {
    project_id: String,
    ordered_active_task_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CreateNoteRequest {
    project_id: String,
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateNoteRequest {
    expected_revision: String,
    title: Option<String>,
    body: Option<String>,
}

#[derive(Debug, Serialize)]
struct PickDirectoryResponse {
    path: String,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

async fn dashboard(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let projects = state
        .service
        .list_projects(200, false)
        .map_err(internal_err)?;
    let selected_project = resolve_selected_project(&projects, query.project_id.as_deref());
    let selected_project_id = selected_project.as_ref().map(|project| project.id.clone());
    let workspace = match selected_project_id.as_deref() {
        Some(project_id) => Some(
            state
                .service
                .load_project_workspace(project_id)
                .map_err(internal_err)?,
        ),
        None => None,
    };

    let initial_state = UiStatePayload {
        projects: projects.into_iter().map(map_project_summary).collect(),
        selected_project_id: selected_project_id.clone(),
        workspace: workspace.map(map_workspace_payload),
        server_port: state.service.config.server.port,
    };

    let template = DashboardTemplate {
        initial_state_json: serde_json::to_string(&initial_state).map_err(internal_err)?,
    };

    Ok(Html(template.render().map_err(internal_err)?))
}

async fn reindex_now(
    State(state): State<Arc<WebState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    state.service.reindex_all().map_err(internal_err)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_projects(State(state): State<Arc<WebState>>) -> ApiResult<Vec<ProjectSummary>> {
    let projects = state
        .service
        .list_projects(200, false)
        .map_err(internal_api_err)?;
    Ok(Json(
        projects.into_iter().map(map_project_summary).collect(),
    ))
}

async fn api_project_workspace(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
) -> ApiResult<WorkspacePayload> {
    let workspace = state
        .service
        .load_project_workspace(&id)
        .map_err(internal_api_err)?;
    Ok(Json(map_workspace_payload(workspace)))
}

async fn api_create_project(
    State(state): State<Arc<WebState>>,
    Json(request): Json<CreateProjectRequest>,
) -> ApiResult<UiStatePayload> {
    let (title, source_kind, source_locator) =
        project_payload_parts(&request).map_err(bad_request_json)?;

    let created = state
        .service
        .create_project(
            CreateProjectPayload {
                title,
                owner: None,
                source_kind,
                source_locator,
                tags: None,
                body: None,
            },
            Actor::human("operator"),
        )
        .map_err(map_service_err_json)?;

    let projects = state
        .service
        .list_projects(200, false)
        .map_err(internal_api_err)?;
    let workspace = state
        .service
        .load_project_workspace(&created.id)
        .map_err(internal_api_err)?;

    Ok(Json(UiStatePayload {
        projects: projects.into_iter().map(map_project_summary).collect(),
        selected_project_id: Some(created.id),
        workspace: Some(map_workspace_payload(workspace)),
        server_port: state.service.config.server.port,
    }))
}

async fn api_create_task(
    State(state): State<Arc<WebState>>,
    Json(request): Json<CreateTaskRequest>,
) -> ApiResult<TaskMutationResponse> {
    let project_id = request.project_id.trim();
    if project_id.is_empty() {
        return Err(bad_request_json("project_id is required"));
    }

    let (workspace, created_task_id) = state
        .service
        .create_task_after(
            project_id,
            request.title,
            request.after_task_id.as_deref(),
            Actor::human("operator"),
        )
        .map_err(map_service_err_json)?;

    Ok(Json(TaskMutationResponse {
        workspace: map_workspace_payload(workspace),
        created_task_id: Some(created_task_id),
    }))
}

async fn api_update_task(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<UpdateTaskRequest>,
) -> ApiResult<TaskMutationResponse> {
    let patch = TaskPatch {
        title: request.title,
        status: parse_task_status(request.status.as_deref()).map_err(bad_request_json)?,
        priority: None,
        assignee: None,
        due_at: None,
        sort_order: None,
        tags: None,
        body: None,
    };

    let updated = state
        .service
        .update_task(
            &id,
            patch,
            &request.expected_revision,
            Actor::human("operator"),
        )
        .map_err(map_service_err_json)?;

    let project_id = updated
        .frontmatter
        .project_id()
        .map(ToString::to_string)
        .ok_or_else(|| bad_request_json("updated task was missing project_id"))?;
    let workspace = state
        .service
        .load_project_workspace(&project_id)
        .map_err(internal_api_err)?;

    Ok(Json(TaskMutationResponse {
        workspace: map_workspace_payload(workspace),
        created_task_id: None,
    }))
}

async fn api_reorder_tasks(
    State(state): State<Arc<WebState>>,
    Json(request): Json<ReorderTasksRequest>,
) -> ApiResult<WorkspacePayload> {
    let workspace = state
        .service
        .reorder_project_tasks(
            &request.project_id,
            &request.ordered_active_task_ids,
            Actor::human("operator"),
        )
        .map_err(map_service_err_json)?;
    Ok(Json(map_workspace_payload(workspace)))
}

async fn api_create_note(
    State(state): State<Arc<WebState>>,
    Json(request): Json<CreateNoteRequest>,
) -> ApiResult<NoteMutationResponse> {
    let project_id = request.project_id.trim();
    if project_id.is_empty() {
        return Err(bad_request_json("project_id is required"));
    }
    let title = normalize_optional(request.title).unwrap_or_else(|| "Untitled note".to_string());

    let created = state
        .service
        .create_note(
            CreateNotePayload {
                title,
                project_id: Some(project_id.to_string()),
                tags: None,
                body: Some(String::new()),
            },
            Actor::human("operator"),
        )
        .map_err(map_service_err_json)?;

    let workspace = state
        .service
        .load_project_workspace(project_id)
        .map_err(internal_api_err)?;

    Ok(Json(NoteMutationResponse {
        workspace: map_workspace_payload(workspace),
        created_note_id: Some(created.id),
    }))
}

async fn api_update_note(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<UpdateNoteRequest>,
) -> ApiResult<NoteMutationResponse> {
    let updated = state
        .service
        .update_note(
            &id,
            NotePatch {
                title: normalize_optional(request.title),
                project_id: None,
                tags: None,
                body: request.body,
            },
            &request.expected_revision,
            Actor::human("operator"),
        )
        .map_err(map_service_err_json)?;

    let project_id = updated
        .frontmatter
        .project_id()
        .map(ToString::to_string)
        .ok_or_else(|| bad_request_json("updated note was missing project_id"))?;
    let workspace = state
        .service
        .load_project_workspace(&project_id)
        .map_err(internal_api_err)?;

    Ok(Json(NoteMutationResponse {
        workspace: map_workspace_payload(workspace),
        created_note_id: None,
    }))
}

async fn api_pick_directory() -> ApiResult<PickDirectoryResponse> {
    let path = pick_directory().map_err(internal_api_err)?;
    Ok(Json(PickDirectoryResponse { path }))
}

fn parse_task_status(value: Option<&str>) -> Result<Option<TaskStatus>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let encoded = format!("\"{value}\"");
    serde_json::from_str::<TaskStatus>(&encoded)
        .map(Some)
        .map_err(|_| format!("invalid task status: {value}"))
}

fn project_payload_parts(
    request: &CreateProjectRequest,
) -> Result<(String, Option<ProjectSourceKind>, Option<String>), String> {
    match request.mode.trim() {
        "new" => {
            let title = normalize_optional(request.title.clone())
                .ok_or_else(|| "project title is required".to_string())?;
            Ok((title, None, None))
        }
        "local" => {
            let raw_locator = normalize_optional(request.source_locator.clone())
                .ok_or_else(|| "source_locator is required".to_string())?;
            let canonical = canonicalize_local_source(&raw_locator)?;
            let title = normalize_optional(request.title.clone())
                .unwrap_or_else(|| derive_title_from_local_path(&canonical));
            Ok((
                title,
                Some(ProjectSourceKind::Local),
                Some(canonical.to_string_lossy().to_string()),
            ))
        }
        "github" => {
            let locator = normalize_optional(request.source_locator.clone())
                .ok_or_else(|| "source_locator is required".to_string())?;
            let title = normalize_optional(request.title.clone())
                .unwrap_or_else(|| derive_title_from_github_locator(&locator));
            Ok((title, Some(ProjectSourceKind::Github), Some(locator)))
        }
        other => Err(format!("unsupported project mode: {other}")),
    }
}

fn resolve_selected_project(
    projects: &[ProjectItem],
    requested: Option<&str>,
) -> Option<ProjectItem> {
    requested
        .and_then(|project_id| projects.iter().find(|project| project.id == project_id))
        .cloned()
        .or_else(|| projects.first().cloned())
}

fn map_workspace_payload(workspace: ProjectWorkspace) -> WorkspacePayload {
    WorkspacePayload {
        project: map_project_summary(workspace.project),
        active_tasks: workspace
            .active_tasks
            .into_iter()
            .map(map_task_payload)
            .collect(),
        done_tasks: workspace
            .done_tasks
            .into_iter()
            .map(map_task_payload)
            .collect(),
        notes: workspace.notes.into_iter().map(map_note_payload).collect(),
        suggested_open_note_id: workspace.suggested_open_note_id,
    }
}

fn map_project_summary(project: ProjectItem) -> ProjectSummary {
    ProjectSummary {
        id: project.id,
        title: project.title,
        source_kind: project.source_kind.map(|kind| kind.as_str().to_string()),
        source_locator: project.source_locator,
    }
}

fn map_task_payload(task: crate::types::TaskItem) -> TaskPayload {
    TaskPayload {
        id: task.id,
        title: task.title,
        revision: task.revision,
        completed_at_label: task.completed_at.map(format_timestamp),
        updated_at_label: format_timestamp(task.updated_at),
        sort_order: task.sort_order,
    }
}

fn map_note_payload(note: crate::types::NoteDetail) -> NotePayload {
    NotePayload {
        id: note.id,
        title: note.title,
        body: note.body.clone(),
        rendered_html: render_markdown_html(&note.body),
        revision: note.revision,
        updated_at_label: format_timestamp(note.updated_at),
    }
}

fn format_timestamp(ts: chrono::DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn canonicalize_local_source(raw_locator: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw_locator);
    let canonical = path
        .canonicalize()
        .map_err(|err| format!("failed to resolve local path: {err}"))?;
    if !canonical.is_dir() {
        return Err("local source must be a directory".to_string());
    }
    Ok(canonical)
}

fn derive_title_from_local_path(path: &PathBuf) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| "Linked project".to_string())
}

fn derive_title_from_github_locator(locator: &str) -> String {
    locator
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("github-project")
        .trim_end_matches(".git")
        .to_string()
}

fn map_service_err_json(err: ServiceError) -> (StatusCode, Json<ApiError>) {
    match err {
        ServiceError::Conflict { expected, current } => (
            StatusCode::CONFLICT,
            Json(ApiError {
                error: "revision conflict".to_string(),
                expected_revision: Some(expected),
                current_revision: Some(current),
            }),
        ),
        ServiceError::Other(err) => internal_api_err(err),
    }
}

fn bad_request_json(message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: message.into(),
            expected_revision: None,
            current_revision: None,
        }),
    )
}

fn internal_api_err(err: impl std::fmt::Display) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError {
            error: err.to_string(),
            expected_revision: None,
            current_revision: None,
        }),
    )
}

fn internal_err(err: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(target_os = "macos")]
fn pick_directory() -> Result<String, anyhow::Error> {
    let output = Command::new("osascript")
        .args([
            "-e",
            r#"POSIX path of (choose folder with prompt "Select project folder")"#,
        ])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "directory picker failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(not(target_os = "macos"))]
fn pick_directory() -> Result<String, anyhow::Error> {
    anyhow::bail!("directory picker is only available on macOS")
}
