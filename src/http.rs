use std::collections::{HashMap, HashSet};
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

use crate::markdown::render_markdown_html;
use crate::repo_sync::derive_sync_source_key;
use crate::service::{AppService, ArchiveEntityRequest, ServiceError};
use crate::types::{
    Actor, CreateNotePayload, CreateProjectPayload, NotePatch, ProjectItem, ProjectPatch,
    ProjectSourceKind, ProjectWorkspace, TaskFilters, TaskPatch, TaskStatus,
};

#[derive(Clone)]
pub struct WebState {
    pub service: Arc<AppService>,
    pub dev_reload_token: Option<String>,
}

pub fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/__dev/reload-token", get(dev_reload_token))
        .route("/reindex", post(reindex_now))
        .route("/api/ui-state", get(api_ui_state))
        .route("/api/archive", get(api_archive_contents))
        .route("/api/archive/empty", post(api_empty_archive))
        .route(
            "/api/archive/{id}/restore",
            post(api_restore_archived_entity),
        )
        .route("/api/projects", get(api_projects).post(api_create_project))
        .route("/api/projects/{id}", patch(api_update_project))
        .route("/api/projects/{id}/archive", post(api_archive_project))
        .route("/api/projects/{id}/sync", post(api_sync_project))
        .route(
            "/api/projects/{id}/task-sync/enable",
            post(api_enable_task_sync),
        )
        .route(
            "/api/projects/{id}/task-sync/pause",
            post(api_pause_task_sync),
        )
        .route(
            "/api/projects/{id}/task-sync/resume",
            post(api_resume_task_sync),
        )
        .route(
            "/api/projects/{id}/task-sync/resolve-file",
            post(api_resolve_task_sync_file),
        )
        .route(
            "/api/projects/{id}/task-sync/resolve-local",
            post(api_resolve_task_sync_local),
        )
        .route(
            "/api/projects/{id}/notes/linkable-files",
            get(api_project_linkable_note_files),
        )
        .route(
            "/api/projects/{id}/notes/link-file",
            post(api_link_note_file),
        )
        .route("/api/projects/{id}/workspace", get(api_project_workspace))
        .route("/api/tasks", post(api_create_task))
        .route("/api/tasks/archive", post(api_archive_tasks))
        .route("/api/tasks/{id}", patch(api_update_task))
        .route("/api/tasks/{id}/archive", post(api_archive_task))
        .route("/api/tasks/reorder", post(api_reorder_tasks))
        .route("/api/notes", post(api_create_note))
        .route("/api/notes/{id}", patch(api_update_note))
        .route(
            "/api/notes/{id}/sync/resolve-file",
            post(api_resolve_note_sync_file),
        )
        .route(
            "/api/notes/{id}/sync/resolve-local",
            post(api_resolve_note_sync_local),
        )
        .route("/api/notes/{id}/archive", post(api_archive_note))
        .route("/api/system/pick-directory", post(api_pick_directory))
        .route("/api/system/open-path", post(api_open_path))
        .with_state(state)
}

#[derive(Template)]
#[template(path = "index.html")]
struct DashboardTemplate {
    initial_state_json: String,
    dev_reload_enabled: bool,
    dev_reload_token: String,
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

#[derive(Debug, Serialize)]
struct ArchiveContentsPayload {
    total_count: usize,
    items: Vec<ArchivedEntityPayload>,
}

#[derive(Debug, Serialize)]
struct ArchivedEntityPayload {
    id: String,
    entity_type: String,
    title: String,
    revision: String,
    context_label: String,
    detail_label: Option<String>,
}

#[derive(Debug, Serialize)]
struct ArchiveMutationResponse {
    archive: ArchiveContentsPayload,
    ui_state: UiStatePayload,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectSummary {
    id: String,
    title: String,
    revision: String,
    source_kind: Option<String>,
    source_locator: Option<String>,
    last_synced_at_label: Option<String>,
    last_sync_summary: Option<String>,
    task_sync_mode: Option<String>,
    task_sync_file: Option<String>,
    task_sync_enabled: bool,
    task_sync_status: Option<String>,
    task_sync_last_inbound_at_label: Option<String>,
    task_sync_last_outbound_at_label: Option<String>,
    task_sync_conflict_summary: Option<String>,
    task_sync_conflict_at_label: Option<String>,
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
    completed_at_iso: Option<String>,
    completed_at_label: Option<String>,
    updated_at_iso: String,
    updated_at_label: String,
    sort_order: i64,
}

#[derive(Debug, Serialize)]
struct NotePayload {
    id: String,
    title: String,
    body: String,
    rendered_html: String,
    sync_kind: Option<String>,
    sync_path: Option<String>,
    sync_status: Option<String>,
    sync_last_inbound_at_iso: Option<String>,
    sync_last_inbound_at_label: Option<String>,
    sync_last_outbound_at_iso: Option<String>,
    sync_last_outbound_at_label: Option<String>,
    sync_conflict_summary: Option<String>,
    sync_conflict_at_label: Option<String>,
    revision: String,
    updated_at_iso: String,
    updated_at_label: String,
}

#[derive(Debug, Serialize)]
struct TaskMutationResponse {
    workspace: WorkspacePayload,
    created_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive: Option<ArchiveContentsPayload>,
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
struct UpdateProjectRequest {
    expected_revision: String,
    current_project_id: Option<String>,
    title: Option<String>,
    source_kind: Option<String>,
    source_locator: Option<String>,
    #[serde(default)]
    clear_source: bool,
}

#[derive(Debug, Deserialize)]
struct ReorderTasksRequest {
    project_id: String,
    ordered_active_task_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ArchiveTasksRequest {
    project_id: String,
    tasks: Vec<ArchiveTaskItemRequest>,
}

#[derive(Debug, Deserialize)]
struct ArchiveTaskItemRequest {
    id: String,
    expected_revision: String,
}

#[derive(Debug, Deserialize)]
struct CreateNoteRequest {
    project_id: String,
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LinkNoteFileRequest {
    relative_path: String,
}

#[derive(Debug, Deserialize)]
struct UpdateNoteRequest {
    expected_revision: String,
    title: Option<String>,
    body: Option<String>,
}

#[derive(Debug, Serialize)]
struct LinkableNoteFilePayload {
    relative_path: String,
    display_name: String,
    display_subpath: String,
}

#[derive(Debug, Serialize)]
struct PickDirectoryResponse {
    path: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedRevisionRequest {
    expected_revision: String,
    current_project_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CurrentProjectRequest {
    current_project_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenPathRequest {
    path: String,
}

#[derive(Debug, Deserialize)]
struct SyncProjectRequest {
    current_project_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct DevReloadTokenPayload {
    token: String,
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
        dev_reload_enabled: state.dev_reload_token.is_some(),
        dev_reload_token: state.dev_reload_token.clone().unwrap_or_default(),
    };

    Ok(Html(template.render().map_err(internal_err)?))
}

async fn dev_reload_token(
    State(state): State<Arc<WebState>>,
) -> Result<Json<DevReloadTokenPayload>, StatusCode> {
    let Some(token) = state.dev_reload_token.clone() else {
        return Err(StatusCode::NOT_FOUND);
    };

    Ok(Json(DevReloadTokenPayload { token }))
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

async fn api_ui_state(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectQuery>,
) -> ApiResult<UiStatePayload> {
    let payload =
        build_ui_state_payload(&state.service, query.project_id).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_archive_contents(
    State(state): State<Arc<WebState>>,
) -> ApiResult<ArchiveContentsPayload> {
    let payload = build_archive_payload(&state.service).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_restore_archived_entity(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<ExpectedRevisionRequest>,
) -> ApiResult<ArchiveMutationResponse> {
    state
        .service
        .restore_entity(&id, &request.expected_revision, Actor::human("operator"))
        .map_err(map_service_err_json)?;

    let ui_state = build_ui_state_payload(&state.service, request.current_project_id)
        .map_err(internal_api_err)?;
    let archive = build_archive_payload(&state.service).map_err(internal_api_err)?;
    Ok(Json(ArchiveMutationResponse { archive, ui_state }))
}

async fn api_empty_archive(
    State(state): State<Arc<WebState>>,
    Json(request): Json<CurrentProjectRequest>,
) -> ApiResult<ArchiveMutationResponse> {
    state
        .service
        .empty_archive()
        .map_err(map_service_err_json)?;

    let ui_state = build_ui_state_payload(&state.service, request.current_project_id)
        .map_err(internal_api_err)?;
    let archive = build_archive_payload(&state.service).map_err(internal_api_err)?;
    Ok(Json(ArchiveMutationResponse { archive, ui_state }))
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

    let payload =
        build_ui_state_payload(&state.service, Some(created.id)).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_update_project(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<UpdateProjectRequest>,
) -> ApiResult<UiStatePayload> {
    let patch = project_patch_from_request(&request).map_err(bad_request_json)?;

    state
        .service
        .update_project(
            &id,
            patch,
            &request.expected_revision,
            Actor::human("operator"),
        )
        .map_err(map_service_err_json)?;

    let selected = request.current_project_id.or_else(|| Some(id.clone()));
    let payload = build_ui_state_payload(&state.service, selected).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_archive_project(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<ExpectedRevisionRequest>,
) -> ApiResult<UiStatePayload> {
    state
        .service
        .archive_entity(&id, &request.expected_revision, Actor::human("operator"))
        .map_err(map_service_err_json)?;

    let payload = build_ui_state_payload(&state.service, request.current_project_id)
        .map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_sync_project(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<SyncProjectRequest>,
) -> ApiResult<UiStatePayload> {
    state
        .service
        .sync_project_from_source(&id, Actor::human("operator"))
        .map_err(map_service_err_json)?;

    let selected = request.current_project_id.or_else(|| Some(id));
    let payload = build_ui_state_payload(&state.service, selected).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_enable_task_sync(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<ExpectedRevisionRequest>,
) -> ApiResult<UiStatePayload> {
    state
        .service
        .enable_managed_task_sync(&id, &request.expected_revision)
        .map_err(map_service_err_json)?;

    let selected = request.current_project_id.or_else(|| Some(id));
    let payload = build_ui_state_payload(&state.service, selected).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_pause_task_sync(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<ExpectedRevisionRequest>,
) -> ApiResult<UiStatePayload> {
    state
        .service
        .pause_managed_task_sync(&id, &request.expected_revision)
        .map_err(map_service_err_json)?;

    let selected = request.current_project_id.or_else(|| Some(id));
    let payload = build_ui_state_payload(&state.service, selected).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_resume_task_sync(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<ExpectedRevisionRequest>,
) -> ApiResult<UiStatePayload> {
    state
        .service
        .resume_managed_task_sync(&id, &request.expected_revision)
        .map_err(map_service_err_json)?;

    let selected = request.current_project_id.or_else(|| Some(id));
    let payload = build_ui_state_payload(&state.service, selected).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_resolve_task_sync_file(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<ExpectedRevisionRequest>,
) -> ApiResult<UiStatePayload> {
    state
        .service
        .resolve_managed_task_sync_from_file(&id, &request.expected_revision)
        .map_err(map_service_err_json)?;

    let selected = request.current_project_id.or_else(|| Some(id));
    let payload = build_ui_state_payload(&state.service, selected).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_resolve_task_sync_local(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<ExpectedRevisionRequest>,
) -> ApiResult<UiStatePayload> {
    state
        .service
        .resolve_managed_task_sync_from_local(&id, &request.expected_revision)
        .map_err(map_service_err_json)?;

    let selected = request.current_project_id.or_else(|| Some(id));
    let payload = build_ui_state_payload(&state.service, selected).map_err(internal_api_err)?;
    Ok(Json(payload))
}

async fn api_project_linkable_note_files(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
) -> ApiResult<Vec<LinkableNoteFilePayload>> {
    let files = state
        .service
        .list_linkable_note_files(&id)
        .map_err(map_service_err_json)?;
    Ok(Json(
        files
            .into_iter()
            .map(map_linkable_note_file_payload)
            .collect(),
    ))
}

async fn api_link_note_file(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<LinkNoteFileRequest>,
) -> ApiResult<NoteMutationResponse> {
    let linked = state
        .service
        .link_note_to_repo_file(&id, &request.relative_path, Actor::human("operator"))
        .map_err(map_service_err_json)?;
    let workspace = state
        .service
        .load_project_workspace(&id)
        .map_err(internal_api_err)?;

    Ok(Json(NoteMutationResponse {
        workspace: map_workspace_payload(workspace),
        created_note_id: Some(linked.id),
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
        archive: None,
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
        archive: None,
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

async fn api_archive_task(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<ExpectedRevisionRequest>,
) -> ApiResult<TaskMutationResponse> {
    let archived = state
        .service
        .archive_entity(&id, &request.expected_revision, Actor::human("operator"))
        .map_err(map_service_err_json)?;

    let project_id = archived
        .frontmatter
        .project_id()
        .map(ToString::to_string)
        .ok_or_else(|| bad_request_json("archived task was missing project_id"))?;
    let workspace = state
        .service
        .load_project_workspace(&project_id)
        .map_err(internal_api_err)?;

    Ok(Json(TaskMutationResponse {
        workspace: map_workspace_payload(workspace),
        created_task_id: None,
        archive: None,
    }))
}

async fn api_archive_tasks(
    State(state): State<Arc<WebState>>,
    Json(request): Json<ArchiveTasksRequest>,
) -> ApiResult<TaskMutationResponse> {
    let project_id = request.project_id.trim();
    if project_id.is_empty() {
        return Err(bad_request_json("project_id is required"));
    }
    if request.tasks.is_empty() {
        return Err(bad_request_json("at least one task is required"));
    }

    let mut archive_requests = Vec::with_capacity(request.tasks.len());
    for task in &request.tasks {
        let task_id = task.id.trim();
        if task_id.is_empty() {
            return Err(bad_request_json("task id is required"));
        }

        let expected_revision = task.expected_revision.trim();
        if expected_revision.is_empty() {
            return Err(bad_request_json("expected_revision is required"));
        }

        archive_requests.push(ArchiveEntityRequest {
            id: task_id.to_string(),
            expected_revision: expected_revision.to_string(),
        });
    }

    state
        .service
        .archive_tasks(archive_requests, Actor::human("operator"))
        .map_err(map_service_err_json)?;

    let workspace = state
        .service
        .load_project_workspace(project_id)
        .map_err(internal_api_err)?;
    let archive = build_archive_payload(&state.service).map_err(internal_api_err)?;

    Ok(Json(TaskMutationResponse {
        workspace: map_workspace_payload(workspace),
        created_task_id: None,
        archive: Some(archive),
    }))
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

async fn api_resolve_note_sync_file(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(_request): Json<ExpectedRevisionRequest>,
) -> ApiResult<NoteMutationResponse> {
    let updated = state
        .service
        .resolve_note_sync_from_file(&id)
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
        created_note_id: Some(id),
    }))
}

async fn api_resolve_note_sync_local(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(_request): Json<ExpectedRevisionRequest>,
) -> ApiResult<NoteMutationResponse> {
    let updated = state
        .service
        .resolve_note_sync_from_local(&id)
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
        created_note_id: Some(id),
    }))
}

async fn api_archive_note(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Json(request): Json<ExpectedRevisionRequest>,
) -> ApiResult<NoteMutationResponse> {
    let archived = state
        .service
        .archive_entity(&id, &request.expected_revision, Actor::human("operator"))
        .map_err(map_service_err_json)?;

    let project_id = archived
        .frontmatter
        .project_id()
        .map(ToString::to_string)
        .ok_or_else(|| bad_request_json("archived note was missing project_id"))?;
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

async fn api_open_path(
    Json(request): Json<OpenPathRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    open_path(&request.path).map_err(internal_api_err)?;
    Ok(StatusCode::NO_CONTENT)
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

fn project_patch_from_request(request: &UpdateProjectRequest) -> Result<ProjectPatch, String> {
    let mut patch = ProjectPatch {
        title: normalize_optional(request.title.clone()),
        ..ProjectPatch::default()
    };

    if request.clear_source {
        patch.source_kind = Some(None);
        patch.source_locator = Some(None);
        patch.sync_source_key = Some(None);
        patch.last_synced_at = Some(None);
        patch.last_sync_summary = Some(None);
        return Ok(patch);
    }

    if let Some(source_kind) = request.source_kind.as_deref() {
        let locator = normalize_optional(request.source_locator.clone())
            .ok_or_else(|| "source_locator is required when setting a source".to_string())?;

        match source_kind {
            "local" => {
                let canonical = canonicalize_local_source(&locator)?;
                patch.source_kind = Some(Some(ProjectSourceKind::Local));
                let canonical_string = canonical.to_string_lossy().to_string();
                patch.source_locator = Some(Some(canonical_string.clone()));
                patch.sync_source_key =
                    Some(Some(derive_sync_source_key("local", &canonical_string)));
                patch.last_synced_at = Some(None);
                patch.last_sync_summary = Some(None);
            }
            "github" => {
                patch.source_kind = Some(Some(ProjectSourceKind::Github));
                patch.sync_source_key = Some(Some(derive_sync_source_key("github", &locator)));
                patch.source_locator = Some(Some(locator));
                patch.last_synced_at = Some(None);
                patch.last_sync_summary = Some(None);
            }
            other => return Err(format!("unsupported source_kind: {other}")),
        }
    }

    Ok(patch)
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

fn build_archive_payload(service: &AppService) -> Result<ArchiveContentsPayload, anyhow::Error> {
    let projects = service.list_projects(5_000, true)?;
    let tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: None,
        assignee: None,
        include_archived: true,
        limit: Some(10_000),
    })?;
    let notes = service.list_notes(10_000, true)?;

    let archived_projects = projects
        .iter()
        .filter(|project| project.archived)
        .cloned()
        .collect::<Vec<_>>();
    let archived_project_ids = archived_projects
        .iter()
        .map(|project| project.id.clone())
        .collect::<HashSet<_>>();
    let active_project_titles = projects
        .iter()
        .filter(|project| !project.archived)
        .map(|project| (project.id.clone(), project.title.clone()))
        .collect::<HashMap<_, _>>();

    let archived_tasks = tasks
        .into_iter()
        .filter(|task| task.archived)
        .collect::<Vec<_>>();
    let archived_notes = notes
        .into_iter()
        .filter(|note| note.archived)
        .collect::<Vec<_>>();

    let mut archived_task_counts = HashMap::<String, usize>::new();
    for task in &archived_tasks {
        *archived_task_counts
            .entry(task.project_id.clone())
            .or_default() += 1;
    }

    let mut archived_note_counts = HashMap::<String, usize>::new();
    for note in &archived_notes {
        if let Some(project_id) = note.project_id.as_ref() {
            *archived_note_counts.entry(project_id.clone()).or_default() += 1;
        }
    }

    let total_count = archived_projects.len() + archived_tasks.len() + archived_notes.len();
    let mut items = Vec::new();

    for project in archived_projects {
        let task_count = archived_task_counts.get(&project.id).copied().unwrap_or(0);
        let note_count = archived_note_counts.get(&project.id).copied().unwrap_or(0);
        items.push(ArchivedEntityPayload {
            id: project.id,
            entity_type: "project".to_string(),
            title: project.title,
            revision: project.revision,
            context_label: "project".to_string(),
            detail_label: format_archive_children_label(task_count, note_count),
        });
    }

    for task in archived_tasks {
        if archived_project_ids.contains(&task.project_id) {
            continue;
        }

        let context_label = active_project_titles
            .get(&task.project_id)
            .map(|title| format!("task in {title}"))
            .unwrap_or_else(|| "task".to_string());
        items.push(ArchivedEntityPayload {
            id: task.id,
            entity_type: "task".to_string(),
            title: task.title,
            revision: task.revision,
            context_label,
            detail_label: None,
        });
    }

    for note in archived_notes {
        if note
            .project_id
            .as_ref()
            .is_some_and(|project_id| archived_project_ids.contains(project_id))
        {
            continue;
        }

        let context_label = note
            .project_id
            .as_ref()
            .and_then(|project_id| active_project_titles.get(project_id))
            .map(|title| format!("note in {title}"))
            .unwrap_or_else(|| "note".to_string());
        items.push(ArchivedEntityPayload {
            id: note.id,
            entity_type: "note".to_string(),
            title: note.title,
            revision: note.revision,
            context_label,
            detail_label: None,
        });
    }

    Ok(ArchiveContentsPayload { total_count, items })
}

fn format_archive_children_label(task_count: usize, note_count: usize) -> Option<String> {
    let mut parts = Vec::new();
    if task_count > 0 {
        let label = if task_count == 1 { "task" } else { "tasks" };
        parts.push(format!("{task_count} {label}"));
    }
    if note_count > 0 {
        let label = if note_count == 1 { "note" } else { "notes" };
        parts.push(format!("{note_count} {label}"));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
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
        revision: project.revision,
        source_kind: project.source_kind.map(|kind| kind.as_str().to_string()),
        source_locator: project.source_locator,
        last_synced_at_label: project.last_synced_at.map(format_timestamp),
        last_sync_summary: project.last_sync_summary,
        task_sync_mode: project.task_sync_mode.map(|mode| mode.as_str().to_string()),
        task_sync_file: project.task_sync_file,
        task_sync_enabled: project.task_sync_enabled,
        task_sync_status: project
            .task_sync_status
            .map(|status| status.as_str().to_string()),
        task_sync_last_inbound_at_label: project.task_sync_last_inbound_at.map(format_timestamp),
        task_sync_last_outbound_at_label: project.task_sync_last_outbound_at.map(format_timestamp),
        task_sync_conflict_summary: project.task_sync_conflict_summary,
        task_sync_conflict_at_label: project.task_sync_conflict_at.map(format_timestamp),
    }
}

fn build_ui_state_payload(
    service: &AppService,
    requested_project_id: Option<String>,
) -> Result<UiStatePayload, anyhow::Error> {
    let projects = service.list_projects(200, false)?;
    let selected_project = resolve_selected_project(&projects, requested_project_id.as_deref());
    let selected_project_id = selected_project.as_ref().map(|project| project.id.clone());
    let workspace = match selected_project_id.as_deref() {
        Some(project_id) => Some(service.load_project_workspace(project_id)?),
        None => None,
    };

    Ok(UiStatePayload {
        projects: projects.into_iter().map(map_project_summary).collect(),
        selected_project_id,
        workspace: workspace.map(map_workspace_payload),
        server_port: service.config.server.port,
    })
}

fn map_task_payload(task: crate::types::TaskItem) -> TaskPayload {
    let completed_at_iso = task.completed_at.as_ref().map(chrono::DateTime::to_rfc3339);
    let completed_at_label = task.completed_at.map(format_timestamp);
    let updated_at_iso = task.updated_at.to_rfc3339();
    let updated_at_label = format_timestamp(task.updated_at);

    TaskPayload {
        id: task.id,
        title: task.title,
        revision: task.revision,
        completed_at_iso,
        completed_at_label,
        updated_at_iso,
        updated_at_label,
        sort_order: task.sort_order,
    }
}

fn map_linkable_note_file_payload(
    item: crate::repo_sync::RepoMarkdownDocCandidate,
) -> LinkableNoteFilePayload {
    LinkableNoteFilePayload {
        relative_path: item.relative_path,
        display_name: item.display_name,
        display_subpath: item.display_subpath,
    }
}

fn map_note_payload(note: crate::types::NoteDetail) -> NotePayload {
    NotePayload {
        id: note.id,
        title: note.title,
        body: note.body.clone(),
        rendered_html: render_markdown_html(&note.body),
        sync_kind: note.sync_kind.map(|kind| kind.as_str().to_string()),
        sync_path: note.sync_path,
        sync_status: note.sync_status.map(|status| status.as_str().to_string()),
        sync_last_inbound_at_iso: note
            .sync_last_inbound_at
            .as_ref()
            .map(chrono::DateTime::to_rfc3339),
        sync_last_inbound_at_label: note.sync_last_inbound_at.map(format_timestamp),
        sync_last_outbound_at_iso: note
            .sync_last_outbound_at
            .as_ref()
            .map(chrono::DateTime::to_rfc3339),
        sync_last_outbound_at_label: note.sync_last_outbound_at.map(format_timestamp),
        sync_conflict_summary: note.sync_conflict_summary,
        sync_conflict_at_label: note.sync_conflict_at.map(format_timestamp),
        revision: note.revision,
        updated_at_iso: note.updated_at.to_rfc3339(),
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

#[cfg(target_os = "macos")]
fn open_path(path: &str) -> Result<(), anyhow::Error> {
    let status = Command::new("open").arg(path).status()?;
    if !status.success() {
        anyhow::bail!("failed opening path");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn pick_directory() -> Result<String, anyhow::Error> {
    anyhow::bail!("directory picker is only available on macOS")
}

#[cfg(not(target_os = "macos"))]
fn open_path(_path: &str) -> Result<(), anyhow::Error> {
    anyhow::bail!("opening local paths is only available on macOS")
}
