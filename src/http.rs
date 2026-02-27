use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Form, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::PollConfig;
use crate::service::{AppService, ServiceError};
use crate::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, EntityType, NoteItem,
    NotePatch, ProjectItem, TaskFilters, TaskItem, TaskPatch, TaskPriority, TaskStatus,
};

#[derive(Clone)]
pub struct WebState {
    pub service: Arc<AppService>,
    pub poll: PollConfig,
}

pub fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/htmx", get(htmx_asset))
        .route("/partials/tasks", get(partial_tasks))
        .route("/partials/projects", get(partial_projects))
        .route("/partials/notes", get(partial_notes))
        .route("/partials/activity", get(partial_activity))
        .route("/reindex", post(reindex_now))
        .route("/projects", post(create_project))
        .route("/tasks", post(create_task))
        .route("/tasks/{id}", patch(update_task).post(update_task))
        .route("/notes", post(create_note))
        .route("/notes/{id}", patch(update_note).post(update_note))
        .route("/archive/{entity_id}", post(archive_entity))
        .with_state(state)
}

#[derive(Template)]
#[template(path = "index.html")]
struct DashboardTemplate {
    tasks: Vec<WorkspaceTaskCard>,
    projects: Vec<WorkspaceProjectCard>,
    notes: Vec<WorkspaceNoteCard>,
    activity: Vec<WorkspaceActivityCard>,
    server_port: u16,
    task_count: usize,
    project_count: usize,
    note_count: usize,
    selected_project_id: String,
    selected_project_title: String,
    selected_project_status: String,
    selected_project_status_class: String,
    selected_project_owner: String,
    selected_project_updated_at: String,
    has_selected_project: bool,
    projects_partial_url: String,
    tasks_partial_url: String,
    notes_partial_url: String,
    has_scope: bool,
    scope_query: String,
    last_activity_at: String,
    latest_activity_summary: String,
    latest_git_branch: String,
    open_task_count: usize,
    done_task_count: usize,
    agent_task_count: usize,
    unassigned_task_count: usize,
    handoff_state: String,
    handoff_detail: String,
    poll_projects_ms: u64,
    poll_tasks_ms: u64,
    poll_notes_ms: u64,
    poll_activity_ms: u64,
}

#[derive(Template)]
#[template(path = "partials/projects.html")]
struct ProjectsTemplate {
    projects: Vec<WorkspaceProjectCard>,
    selected_project_id: String,
}

#[derive(Template)]
#[template(path = "partials/tasks.html")]
struct TasksTemplate {
    tasks: Vec<WorkspaceTaskCard>,
    has_scope: bool,
    scope_query: String,
}

#[derive(Template)]
#[template(path = "partials/notes.html")]
struct NotesTemplate {
    notes: Vec<WorkspaceNoteCard>,
    has_scope: bool,
}

#[derive(Template)]
#[template(path = "partials/activity.html")]
struct ActivityTemplate {
    activity: Vec<WorkspaceActivityCard>,
}

#[derive(Clone)]
struct WorkspaceProjectCard {
    id: String,
    title: String,
    status: String,
    status_class: String,
    owner_label: String,
    updated_label: String,
}

#[derive(Clone)]
struct WorkspaceTaskCard {
    id: String,
    title: String,
    status: String,
    status_class: String,
    priority: String,
    priority_class: String,
    assignee: String,
    assignee_class: String,
    due_label: String,
    updated_label: String,
    revision: String,
    is_done: bool,
}

#[derive(Clone)]
struct WorkspaceNoteCard {
    id: String,
    title: String,
    path: String,
    project_label: String,
    updated_label: String,
}

#[derive(Clone)]
struct WorkspaceActivityCard {
    actor_label: String,
    actor_class: String,
    action_label: String,
    entity_label: String,
    summary: String,
    occurred_label: String,
    git_label: String,
}

#[derive(Deserialize)]
pub struct CreateProjectForm {
    pub title: String,
    pub owner: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateTaskForm {
    pub title: String,
    pub project_id: String,
    pub assignee: Option<String>,
    pub priority: Option<String>,
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateTaskForm {
    pub expected_revision: String,
    pub title: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub assignee: Option<String>,
    pub due_at: Option<String>,
    pub body: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateNoteForm {
    pub title: String,
    pub project_id: Option<String>,
    pub body: Option<String>,
    pub ui_project_id: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateNoteForm {
    pub expected_revision: String,
    pub title: Option<String>,
    pub project_id: Option<String>,
    pub body: Option<String>,
}

#[derive(Deserialize)]
pub struct ArchiveForm {
    pub expected_revision: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProjectScopeQuery {
    project_id: Option<String>,
}

impl ProjectScopeQuery {
    fn project_id(&self) -> Option<String> {
        normalize_optional(self.project_id.clone())
    }
}

async fn dashboard(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectScopeQuery>,
) -> Result<Response, (StatusCode, String)> {
    let project_items = state
        .service
        .list_projects(200, false)
        .map_err(internal_err)?;
    let requested_project_id = query.project_id();
    let selected_project =
        resolve_selected_project(&project_items, requested_project_id.as_deref());
    let selected_project_id = selected_project.as_ref().map(|project| project.id.clone());

    let task_items = list_scoped_tasks(&state.service, selected_project_id.as_deref())?;
    let note_items = list_scoped_notes(&state.service, selected_project_id.as_deref(), 200)?;

    let activity_items = state
        .service
        .list_recent_activity(None, 100)
        .map_err(internal_err)?;

    let open_task_count = task_items
        .iter()
        .filter(|task| task.status != TaskStatus::Done)
        .count();
    let done_task_count = task_items.len().saturating_sub(open_task_count);
    let agent_task_count = task_items
        .iter()
        .filter(|task| task.assignee.starts_with("agent:"))
        .count();
    let unassigned_task_count = task_items
        .iter()
        .filter(|task| task.assignee == "agent:unassigned")
        .count();
    let (handoff_state, handoff_detail) = handoff_summary(&task_items, &note_items);

    let latest_activity_summary = activity_items
        .first()
        .map(|item| item.summary.clone())
        .unwrap_or_else(|| "No mutations recorded yet.".to_string());
    let latest_git_branch = activity_items
        .iter()
        .find_map(|item| item.git_branch.clone())
        .unwrap_or_else(|| "git context unavailable".to_string());

    let template = DashboardTemplate {
        server_port: state.service.config.server.port,
        task_count: task_items.len(),
        project_count: project_items.len(),
        note_count: note_items.len(),
        selected_project_id: selected_project_id.clone().unwrap_or_default(),
        selected_project_title: selected_project
            .as_ref()
            .map(|project| project.title.clone())
            .unwrap_or_else(|| "Select a project".to_string()),
        selected_project_status: selected_project
            .as_ref()
            .map(|project| project.status.clone())
            .unwrap_or_else(|| "none".to_string()),
        selected_project_status_class: selected_project
            .as_ref()
            .map(|project| status_class_for_label(&project.status))
            .unwrap_or_else(|| "status-neutral".to_string()),
        selected_project_owner: selected_project
            .as_ref()
            .and_then(|project| project.owner.clone())
            .unwrap_or_else(|| "unowned".to_string()),
        selected_project_updated_at: selected_project
            .as_ref()
            .map(|project| format_timestamp(project.updated_at))
            .unwrap_or_else(|| "never".to_string()),
        has_selected_project: selected_project.is_some(),
        projects_partial_url: scoped_path("/partials/projects", selected_project_id.as_deref()),
        tasks_partial_url: scoped_path("/partials/tasks", selected_project_id.as_deref()),
        notes_partial_url: scoped_path("/partials/notes", selected_project_id.as_deref()),
        has_scope: selected_project.is_some(),
        scope_query: scope_query_suffix(selected_project_id.as_deref()),
        last_activity_at: activity_items
            .first()
            .map(|item| format_timestamp(item.occurred_at))
            .unwrap_or_else(|| "never".to_string()),
        latest_activity_summary,
        latest_git_branch,
        open_task_count,
        done_task_count,
        agent_task_count,
        unassigned_task_count,
        handoff_state,
        handoff_detail,
        tasks: task_items.into_iter().map(map_task_card).collect(),
        projects: project_items.into_iter().map(map_project_card).collect(),
        notes: note_items.into_iter().map(map_note_card).collect(),
        activity: activity_items.into_iter().map(map_activity_card).collect(),
        poll_projects_ms: state.poll.tasks_interval_ms,
        poll_tasks_ms: state.poll.tasks_interval_ms,
        poll_notes_ms: state.poll.notes_interval_ms,
        poll_activity_ms: state.poll.activity_interval_ms,
    };

    let html = template.render().map_err(internal_err)?;
    Ok(Html(html).into_response())
}

async fn htmx_asset() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../assets/htmx.min.js"),
    )
}

async fn partial_tasks(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectScopeQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let project_id = query.project_id();
    let tasks = list_scoped_tasks(&state.service, project_id.as_deref())?;

    render_with_etag(
        TasksTemplate {
            has_scope: project_id.is_some(),
            scope_query: scope_query_suffix(project_id.as_deref()),
            tasks: tasks.into_iter().map(map_task_card).collect(),
        }
        .render()
        .map_err(internal_err)?,
        &headers,
    )
}

async fn partial_projects(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectScopeQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let projects = state
        .service
        .list_projects(200, false)
        .map_err(internal_err)?;
    render_with_etag(
        ProjectsTemplate {
            projects: projects.into_iter().map(map_project_card).collect(),
            selected_project_id: query.project_id().unwrap_or_default(),
        }
        .render()
        .map_err(internal_err)?,
        &headers,
    )
}

async fn partial_notes(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectScopeQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let project_id = query.project_id();
    let notes = list_scoped_notes(&state.service, project_id.as_deref(), 200)?;
    render_with_etag(
        NotesTemplate {
            has_scope: project_id.is_some(),
            notes: notes.into_iter().map(map_note_card).collect(),
        }
        .render()
        .map_err(internal_err)?,
        &headers,
    )
}

async fn partial_activity(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let since: Option<DateTime<Utc>> = None;
    let activity = state
        .service
        .list_recent_activity(since, 100)
        .map_err(internal_err)?;
    render_with_etag(
        ActivityTemplate {
            activity: activity.into_iter().map(map_activity_card).collect(),
        }
        .render()
        .map_err(internal_err)?,
        &headers,
    )
}

async fn reindex_now(State(state): State<Arc<WebState>>) -> Result<Response, (StatusCode, String)> {
    state.service.reindex_all().map_err(internal_err)?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        HeaderName::from_static("hx-trigger"),
        HeaderValue::from_static("n10e-refresh"),
    );
    Ok(response)
}

async fn create_project(
    State(state): State<Arc<WebState>>,
    Form(form): Form<CreateProjectForm>,
) -> Result<Response, (StatusCode, String)> {
    let title = form.title.trim();
    if title.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "project title is required".to_string(),
        ));
    }

    let created = state
        .service
        .create_project(
            CreateProjectPayload {
                title: title.to_string(),
                owner: normalize_optional(form.owner),
                tags: None,
                body: None,
            },
            Actor::human("operator"),
        )
        .map_err(map_service_err)?;

    let projects = state
        .service
        .list_projects(200, false)
        .map_err(internal_err)?;
    Ok(Html(
        ProjectsTemplate {
            projects: projects.into_iter().map(map_project_card).collect(),
            selected_project_id: created.id,
        }
        .render()
        .map_err(internal_err)?,
    )
    .into_response())
}

async fn create_task(
    State(state): State<Arc<WebState>>,
    Form(form): Form<CreateTaskForm>,
) -> Result<Response, (StatusCode, String)> {
    let project_id = form.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "project_id is required".to_string(),
        ));
    }
    ensure_project_exists_for_ui(&state.service, &project_id)?;

    let payload = CreateTaskPayload {
        title: form.title.trim().to_string(),
        project_id: project_id.clone(),
        status: parse_task_status(form.status.as_deref())?,
        priority: parse_task_priority(form.priority.as_deref())?,
        assignee: normalize_optional(form.assignee),
        due_at: None,
        tags: None,
        body: None,
    };

    state
        .service
        .create_task(payload, Actor::human("operator"))
        .map_err(map_service_err)?;

    let tasks = list_scoped_tasks(&state.service, Some(&project_id))?;

    Ok(Html(
        TasksTemplate {
            has_scope: true,
            scope_query: scope_query_suffix(Some(&project_id)),
            tasks: tasks.into_iter().map(map_task_card).collect(),
        }
        .render()
        .map_err(internal_err)?,
    )
    .into_response())
}

async fn update_task(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectScopeQuery>,
    Form(form): Form<UpdateTaskForm>,
) -> Result<Response, (StatusCode, String)> {
    let patch = TaskPatch {
        title: form.title,
        status: parse_task_status(form.status.as_deref())?,
        priority: parse_task_priority(form.priority.as_deref())?,
        assignee: form.assignee,
        due_at: form.due_at,
        tags: None,
        body: form.body,
    };

    state
        .service
        .update_task(
            &id,
            patch,
            &form.expected_revision,
            Actor::human("operator"),
        )
        .map_err(map_service_err)?;

    let project_id = query.project_id();
    let tasks = list_scoped_tasks(&state.service, project_id.as_deref())?;

    Ok(Html(
        TasksTemplate {
            has_scope: project_id.is_some(),
            scope_query: scope_query_suffix(project_id.as_deref()),
            tasks: tasks.into_iter().map(map_task_card).collect(),
        }
        .render()
        .map_err(internal_err)?,
    )
    .into_response())
}

async fn create_note(
    State(state): State<Arc<WebState>>,
    Form(form): Form<CreateNoteForm>,
) -> Result<Response, (StatusCode, String)> {
    let project_id = normalize_optional(form.project_id);
    if let Some(project_id) = project_id.as_deref() {
        ensure_project_exists_for_ui(&state.service, project_id)?;
    }

    let payload = CreateNotePayload {
        title: form.title.trim().to_string(),
        project_id: project_id.clone(),
        tags: None,
        body: normalize_optional(form.body),
    };

    state
        .service
        .create_note(payload, Actor::human("operator"))
        .map_err(map_service_err)?;

    let scope_project_id = normalize_optional(form.ui_project_id).or(project_id);
    let notes = list_scoped_notes(&state.service, scope_project_id.as_deref(), 200)?;

    Ok(Html(
        NotesTemplate {
            has_scope: scope_project_id.is_some(),
            notes: notes.into_iter().map(map_note_card).collect(),
        }
        .render()
        .map_err(internal_err)?,
    )
    .into_response())
}

async fn update_note(
    Path(id): Path<String>,
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectScopeQuery>,
    Form(form): Form<UpdateNoteForm>,
) -> Result<Response, (StatusCode, String)> {
    let project_id = normalize_optional(form.project_id);
    if let Some(project_id) = project_id.as_deref() {
        ensure_project_exists_for_ui(&state.service, project_id)?;
    }

    let patch = NotePatch {
        title: normalize_optional(form.title),
        project_id,
        tags: None,
        body: normalize_optional(form.body),
    };

    state
        .service
        .update_note(
            &id,
            patch,
            &form.expected_revision,
            Actor::human("operator"),
        )
        .map_err(map_service_err)?;

    let project_id = query.project_id();
    let notes = list_scoped_notes(&state.service, project_id.as_deref(), 200)?;

    Ok(Html(
        NotesTemplate {
            has_scope: project_id.is_some(),
            notes: notes.into_iter().map(map_note_card).collect(),
        }
        .render()
        .map_err(internal_err)?,
    )
    .into_response())
}

async fn archive_entity(
    Path(entity_id): Path<String>,
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectScopeQuery>,
    Form(form): Form<ArchiveForm>,
) -> Result<Response, (StatusCode, String)> {
    state
        .service
        .archive_entity(
            &entity_id,
            &form.expected_revision,
            Actor::human("operator"),
        )
        .map_err(map_service_err)?;

    let project_id = query.project_id();
    let tasks = list_scoped_tasks(&state.service, project_id.as_deref())?;

    Ok(Html(
        TasksTemplate {
            has_scope: project_id.is_some(),
            scope_query: scope_query_suffix(project_id.as_deref()),
            tasks: tasks.into_iter().map(map_task_card).collect(),
        }
        .render()
        .map_err(internal_err)?,
    )
    .into_response())
}

fn parse_task_status(value: Option<&str>) -> Result<Option<TaskStatus>, (StatusCode, String)> {
    let Some(value) = value else {
        return Ok(None);
    };
    let encoded = format!("\"{}\"", value);
    serde_json::from_str::<TaskStatus>(&encoded)
        .map(Some)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid task status: {value}"),
            )
        })
}

fn parse_task_priority(value: Option<&str>) -> Result<Option<TaskPriority>, (StatusCode, String)> {
    let Some(value) = value else {
        return Ok(None);
    };
    let encoded = format!("\"{}\"", value);
    serde_json::from_str::<TaskPriority>(&encoded)
        .map(Some)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid task priority: {value}"),
            )
        })
}

fn render_with_etag(
    content: String,
    headers: &HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let etag = make_etag(&content);

    if headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value == etag)
        .unwrap_or(false)
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }

    let mut response = Html(content).into_response();
    response.headers_mut().insert(
        axum::http::header::ETAG,
        HeaderValue::from_str(&etag).map_err(internal_err)?,
    );
    Ok(response)
}

fn make_etag(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("\"{:x}\"", hasher.finalize())
}

fn map_service_err(err: ServiceError) -> (StatusCode, String) {
    match err {
        ServiceError::Conflict { expected, current } => (
            StatusCode::CONFLICT,
            format!("revision conflict; expected={expected} current={current}"),
        ),
        ServiceError::Other(err) => {
            let msg = err.to_string();
            if msg.contains("not found") || msg.contains("outside workspace") {
                (StatusCode::BAD_REQUEST, msg)
            } else {
                internal_err(msg)
            }
        }
    }
}

fn internal_err(err: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn ensure_project_exists_for_ui(
    service: &AppService,
    project_id: &str,
) -> Result<(), (StatusCode, String)> {
    match service.read_entity(project_id).map_err(internal_err)? {
        Some(entity) if entity.entity_type == EntityType::Project => Ok(()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("unknown project_id '{project_id}'"),
        )),
    }
}

fn resolve_selected_project(
    projects: &[ProjectItem],
    requested: Option<&str>,
) -> Option<ProjectItem> {
    if let Some(requested) = requested {
        if let Some(project) = projects.iter().find(|project| project.id == requested) {
            return Some(project.clone());
        }
    }
    projects.first().cloned()
}

fn list_scoped_tasks(
    service: &AppService,
    project_id: Option<&str>,
) -> Result<Vec<TaskItem>, (StatusCode, String)> {
    service
        .list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: project_id.map(ToOwned::to_owned),
            assignee: None,
            include_archived: false,
            limit: Some(200),
        })
        .map_err(internal_err)
}

fn list_scoped_notes(
    service: &AppService,
    project_id: Option<&str>,
    limit: usize,
) -> Result<Vec<NoteItem>, (StatusCode, String)> {
    let mut notes = service.list_notes(limit, false).map_err(internal_err)?;
    if let Some(project_id) = project_id {
        notes.retain(|note| note.project_id.as_deref() == Some(project_id));
    }
    Ok(notes)
}

fn scope_query_suffix(project_id: Option<&str>) -> String {
    project_id
        .map(|project_id| format!("?project_id={project_id}"))
        .unwrap_or_default()
}

fn scoped_path(base: &str, project_id: Option<&str>) -> String {
    format!("{base}{}", scope_query_suffix(project_id))
}

fn map_project_card(project: ProjectItem) -> WorkspaceProjectCard {
    WorkspaceProjectCard {
        id: project.id,
        title: project.title,
        status_class: status_class_for_label(&project.status),
        status: project.status,
        owner_label: project.owner.unwrap_or_else(|| "owner unset".to_string()),
        updated_label: format_timestamp(project.updated_at),
    }
}

fn map_task_card(task: TaskItem) -> WorkspaceTaskCard {
    let priority = task.priority.as_str().to_string();
    let status = task.status.as_str().to_string();
    let assignee = task.assignee;
    let due_label = task
        .due_at
        .map(format_timestamp)
        .unwrap_or_else(|| "no due date".to_string());
    let is_done = task.status == TaskStatus::Done;

    WorkspaceTaskCard {
        id: task.id,
        title: task.title,
        status_class: status_class_for_label(&status),
        status,
        priority_class: priority_class_for_label(&priority),
        priority,
        assignee_class: assignee_class_for_label(&assignee),
        assignee,
        due_label,
        updated_label: format_timestamp(task.updated_at),
        revision: task.revision,
        is_done,
    }
}

fn map_note_card(note: NoteItem) -> WorkspaceNoteCard {
    WorkspaceNoteCard {
        id: note.id,
        title: note.title,
        path: note.path,
        project_label: note
            .project_id
            .unwrap_or_else(|| "unscoped note".to_string()),
        updated_label: format_timestamp(note.updated_at),
    }
}

fn map_activity_card(item: crate::types::ActivityItem) -> WorkspaceActivityCard {
    let actor_label = format!("{}:{}", item.actor_kind, item.actor_id);
    let entity_label = item.entity_id.unwrap_or_else(|| "workspace".to_string());
    let git_label = match (item.git_branch, item.git_commit) {
        (Some(branch), Some(commit)) => format!("{branch} @ {}", shorten_commit(&commit)),
        (Some(branch), None) => branch,
        _ => "git context unavailable".to_string(),
    };

    WorkspaceActivityCard {
        actor_class: actor_class_for_label(&item.actor_kind),
        actor_label,
        action_label: item.action,
        entity_label,
        summary: item.summary,
        occurred_label: format_timestamp(item.occurred_at),
        git_label,
    }
}

fn format_timestamp(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn shorten_commit(commit: &str) -> String {
    commit.chars().take(8).collect()
}

fn status_class_for_label(status: &str) -> String {
    match status {
        "active" | "in_progress" => "status-live".to_string(),
        "done" => "status-done".to_string(),
        "blocked" => "status-warn".to_string(),
        "backlog" | "todo" => "status-neutral".to_string(),
        _ => "status-neutral".to_string(),
    }
}

fn priority_class_for_label(priority: &str) -> String {
    match priority {
        "P0" => "priority-p0".to_string(),
        "P1" => "priority-p1".to_string(),
        "P2" => "priority-p2".to_string(),
        _ => "priority-p3".to_string(),
    }
}

fn assignee_class_for_label(assignee: &str) -> String {
    if assignee.starts_with("agent:") {
        "assignee-agent".to_string()
    } else if assignee.starts_with("human:") {
        "assignee-human".to_string()
    } else {
        "assignee-neutral".to_string()
    }
}

fn actor_class_for_label(actor_kind: &str) -> String {
    match actor_kind {
        "agent" => "actor-agent".to_string(),
        "human" => "actor-human".to_string(),
        _ => "actor-system".to_string(),
    }
}

fn handoff_summary(tasks: &[TaskItem], notes: &[NoteItem]) -> (String, String) {
    if tasks.is_empty() && notes.is_empty() {
        return (
            "cold start".to_string(),
            "Add a plan and a few notes before handing work to an agent.".to_string(),
        );
    }

    if tasks.is_empty() {
        return (
            "notes only".to_string(),
            "Context exists, but execution still needs concrete tasks.".to_string(),
        );
    }

    if notes.is_empty() {
        return (
            "task heavy".to_string(),
            "Work is queued, but shared context is still thin.".to_string(),
        );
    }

    let agent_owned = tasks
        .iter()
        .filter(|task| task.assignee.starts_with("agent:"))
        .count();

    if agent_owned == 0 {
        return (
            "ready to assign".to_string(),
            "The plan is documented. Assign an agent when you want execution to start.".to_string(),
        );
    }

    (
        "handoff ready".to_string(),
        "Tasks and notes give agents enough context to pick up quickly.".to_string(),
    )
}
