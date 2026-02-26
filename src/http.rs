use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
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
    tasks: Vec<TaskItem>,
    projects: Vec<ProjectItem>,
    notes: Vec<NoteItem>,
    activity: Vec<crate::types::ActivityItem>,
    server_port: u16,
    task_count: usize,
    project_count: usize,
    note_count: usize,
    selected_project_id: String,
    selected_project_title: String,
    selected_project_status: String,
    has_selected_project: bool,
    projects_partial_url: String,
    tasks_partial_url: String,
    notes_partial_url: String,
    has_scope: bool,
    scope_query: String,
    poll_projects_ms: u64,
    poll_tasks_ms: u64,
    poll_notes_ms: u64,
    poll_activity_ms: u64,
}

#[derive(Template)]
#[template(path = "partials/projects.html")]
struct ProjectsTemplate {
    projects: Vec<ProjectItem>,
    selected_project_id: String,
}

#[derive(Template)]
#[template(path = "partials/tasks.html")]
struct TasksTemplate {
    tasks: Vec<TaskItem>,
    has_scope: bool,
    scope_query: String,
}

#[derive(Template)]
#[template(path = "partials/notes.html")]
struct NotesTemplate {
    notes: Vec<NoteItem>,
    has_scope: bool,
}

#[derive(Template)]
#[template(path = "partials/activity.html")]
struct ActivityTemplate {
    activity: Vec<crate::types::ActivityItem>,
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
    let projects = state
        .service
        .list_projects(200, false)
        .map_err(internal_err)?;
    let requested_project_id = query.project_id();
    let selected_project = resolve_selected_project(&projects, requested_project_id.as_deref());
    let selected_project_id = selected_project.as_ref().map(|project| project.id.clone());

    let tasks = list_scoped_tasks(&state.service, selected_project_id.as_deref())?;
    let notes = list_scoped_notes(&state.service, selected_project_id.as_deref(), 200)?;

    let activity = state
        .service
        .list_recent_activity(None, 100)
        .map_err(internal_err)?;

    let template = DashboardTemplate {
        server_port: state.service.config.server.port,
        task_count: tasks.len(),
        project_count: projects.len(),
        note_count: notes.len(),
        selected_project_id: selected_project_id.clone().unwrap_or_default(),
        selected_project_title: selected_project
            .as_ref()
            .map(|project| project.title.clone())
            .unwrap_or_else(|| "Select a project".to_string()),
        selected_project_status: selected_project
            .as_ref()
            .map(|project| project.status.clone())
            .unwrap_or_else(|| "none".to_string()),
        has_selected_project: selected_project.is_some(),
        projects_partial_url: scoped_path("/partials/projects", selected_project_id.as_deref()),
        tasks_partial_url: scoped_path("/partials/tasks", selected_project_id.as_deref()),
        notes_partial_url: scoped_path("/partials/notes", selected_project_id.as_deref()),
        has_scope: selected_project.is_some(),
        scope_query: scope_query_suffix(selected_project_id.as_deref()),
        tasks,
        projects,
        notes,
        activity,
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
            tasks,
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
            projects,
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
            notes,
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
        ActivityTemplate { activity }
            .render()
            .map_err(internal_err)?,
        &headers,
    )
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
            projects,
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
            tasks,
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
            tasks,
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
            notes,
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
            notes,
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
            tasks,
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
