mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tower::util::ServiceExt;

use topside::codex::CodexSessionManager;
use topside::codex::CodexSessionRecord;
use topside::http::{WebState, router};
use topside::ports::{
    PortManager, PortManagerError, PortSession, TerminatePortResult, UnsupportedPortManager,
};
use topside::service::AppService;
use topside::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, ProjectSourceKind,
    TaskFilters, TaskStatus, TaskSyncMode, TaskSyncStatus,
};

#[derive(Default)]
struct FakePortManagerState {
    list_result: Option<Result<Vec<PortSession>, PortManagerError>>,
    terminate_result: Option<Result<TerminatePortResult, PortManagerError>>,
    list_calls: usize,
    terminate_calls: Vec<(u32, u16)>,
}

#[derive(Clone, Default)]
struct FakePortManager {
    state: Arc<Mutex<FakePortManagerState>>,
}

impl FakePortManager {
    fn new(
        list_result: Result<Vec<PortSession>, PortManagerError>,
        terminate_result: Result<TerminatePortResult, PortManagerError>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakePortManagerState {
                list_result: Some(list_result),
                terminate_result: Some(terminate_result),
                list_calls: 0,
                terminate_calls: Vec::new(),
            })),
        }
    }

    fn snapshot(&self) -> (usize, Vec<(u32, u16)>) {
        let state = self.state.lock().unwrap();
        (state.list_calls, state.terminate_calls.clone())
    }
}

impl PortManager for FakePortManager {
    fn list_sessions(&self) -> Result<Vec<PortSession>, PortManagerError> {
        let mut state = self.state.lock().unwrap();
        state.list_calls += 1;
        state.list_result.clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    fn terminate_session(
        &self,
        pid: u32,
        port: u16,
    ) -> Result<TerminatePortResult, PortManagerError> {
        let mut state = self.state.lock().unwrap();
        state.terminate_calls.push((pid, port));
        state.terminate_result.clone().unwrap_or_else(|| {
            Ok(TerminatePortResult {
                items: Vec::new(),
                message: String::new(),
            })
        })
    }
}

fn build_test_state(
    service: &AppService,
    dev_reload_token: Option<String>,
    port_manager: Arc<dyn PortManager>,
) -> Result<Arc<WebState>> {
    let service = Arc::new(service.clone());
    Ok(Arc::new(WebState {
        codex_manager: Arc::new(CodexSessionManager::new(service.clone())?),
        service,
        dev_reload_token,
        port_manager,
    }))
}

fn codex_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

struct MockCodexEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    _tmp: TempDir,
    previous_bin: Option<String>,
    previous_home: Option<String>,
}

impl MockCodexEnv {
    fn new() -> Result<Self> {
        let guard = codex_env_lock();
        let tmp = TempDir::new()?;
        let home_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(home_dir.join("sessions"))?;

        let script_path = tmp.path().join("mock-codex.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
set -eu
cwd=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-C" ]; then
    cwd="$arg"
  fi
  prev="$arg"
done
if [ -z "$cwd" ]; then
  cwd="$(pwd)"
fi
ts="$(date -u +"%Y-%m-%dT%H:%M:%S+00:00")"
session_id="$(uuidgen | tr 'A-Z' 'a-z')"
session_dir="${TOPSIDE_CODEX_HOME}/sessions/$(date -u +%Y/%m/%d)"
mkdir -p "$session_dir"
printf '{"id":"%s","thread_name":"Mock Codex Session","updated_at":"%s"}\n' "$session_id" "$ts" >> "${TOPSIDE_CODEX_HOME}/session_index.jsonl"
printf '{"type":"session_meta","timestamp":"%s","payload":{"id":"%s","cwd":"%s","timestamp":"%s"}}\n' "$ts" "$session_id" "$cwd" "$ts" > "$session_dir/$session_id.jsonl"
printf 'mock-codex:%s\r\n' "$session_id"
cat
"#,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms)?;
        }

        let previous_bin = std::env::var("TOPSIDE_CODEX_BIN").ok();
        let previous_home = std::env::var("TOPSIDE_CODEX_HOME").ok();
        unsafe {
            std::env::set_var("TOPSIDE_CODEX_BIN", &script_path);
            std::env::set_var("TOPSIDE_CODEX_HOME", &home_dir);
        }

        Ok(Self {
            _guard: guard,
            _tmp: tmp,
            previous_bin,
            previous_home,
        })
    }
}

impl Drop for MockCodexEnv {
    fn drop(&mut self) {
        if let Some(value) = &self.previous_bin {
            unsafe {
                std::env::set_var("TOPSIDE_CODEX_BIN", value);
            }
        } else {
            unsafe {
                std::env::remove_var("TOPSIDE_CODEX_BIN");
            }
        }
        if let Some(value) = &self.previous_home {
            unsafe {
                std::env::set_var("TOPSIDE_CODEX_HOME", value);
            }
        } else {
            unsafe {
                std::env::remove_var("TOPSIDE_CODEX_HOME");
            }
        }
    }
}

fn desktop_request(builder: axum::http::request::Builder) -> axum::http::request::Builder {
    builder.header("x-topside-desktop", "true")
}

async fn wait_for_codex_session<F>(
    service: &AppService,
    project_id: &str,
    session_id: &str,
    mut predicate: F,
) -> Result<CodexSessionRecord>
where
    F: FnMut(&CodexSessionRecord) -> bool,
{
    for _ in 0..80 {
        let workspace = service.load_project_workspace(project_id)?;
        if let Some(session) = workspace
            .codex_sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
        {
            if predicate(&session) {
                return Ok(session);
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    anyhow::bail!("timed out waiting for codex session {session_id}")
}

async fn wait_for_codex_session_absent(
    service: &AppService,
    project_id: &str,
    session_id: &str,
) -> Result<()> {
    for _ in 0..80 {
        let workspace = service.load_project_workspace(project_id)?;
        if !workspace
            .codex_sessions
            .iter()
            .any(|session| session.id == session_id)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    anyhow::bail!("timed out waiting for codex session {session_id} to disappear")
}

#[tokio::test]
async fn dashboard_and_api_workspace_endpoints_work() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let _project = service.create_project(
        CreateProjectPayload {
            title: "HTTP Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            icon: None,
            tags: None,
            body: Some("http body".to_string()),
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state.clone());

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let html = String::from_utf8(body.to_vec())?;
    assert!(html.contains("projects"));

    let response_1 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/projects")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response_1.status(), StatusCode::OK);
    let body = to_bytes(response_1.into_body(), usize::MAX).await?;
    let json = String::from_utf8(body.to_vec())?;
    assert!(json.contains("HTTP Project"));

    Ok(())
}

#[tokio::test]
async fn codex_discover_endpoint_is_not_exposed() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/projects/prj_hidden/codex-sessions/discover")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn task_http_mutations_and_conflict_path() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Mutation Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let create_json = format!(
        "{{\"project_id\":\"{}\",\"title\":\"HTTP Task\",\"after_task_id\":null}}",
        project.id
    );

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tasks")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_json))?,
        )
        .await?;
    assert_eq!(create_response.status(), StatusCode::OK);

    let tasks = service.list_tasks(&topside::types::TaskFilters {
        status: None,
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(10),
    })?;
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task.priority.as_str(), "P0");

    let update_json = format!(
        "{{\"expected_revision\":\"{}\",\"status\":\"done\"}}",
        task.revision
    );

    let update_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/tasks/{}", task.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(update_json))?,
        )
        .await?;
    assert_eq!(update_response.status(), StatusCode::OK);

    let stale_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/tasks/{}", task.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\",\"status\":\"in_progress\"}}",
                    task.revision
                )))?,
        )
        .await?;

    assert_eq!(stale_response.status(), StatusCode::CONFLICT);

    let updated_tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(10),
    })?;
    assert_eq!(updated_tasks.len(), 1);

    let archive_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/tasks/{}/archive", task.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\"}}",
                    updated_tasks[0].revision
                )))?,
        )
        .await?;
    assert_eq!(archive_response.status(), StatusCode::OK);

    let remaining_tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(10),
    })?;
    assert!(remaining_tasks.is_empty());
    Ok(())
}

#[tokio::test]
async fn note_archive_endpoint_removes_note_from_workspace() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Note Archive Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let note = service.create_note(
        CreateNotePayload {
            title: "HTTP Note".to_string(),
            project_id: Some(project.id.clone()),
            tags: None,
            body: Some("archive me".to_string()),
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let archive_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/notes/{}/archive", note.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\"}}",
                    note.revision
                )))?,
        )
        .await?;
    assert_eq!(archive_response.status(), StatusCode::OK);

    let workspace = service.load_project_workspace(&project.id)?;
    assert!(workspace.notes.is_empty());

    let notes = service.list_notes(10, true)?;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].id, note.id);
    assert!(notes[0].archived);

    Ok(())
}

#[tokio::test]
async fn linked_note_endpoints_list_and_link_repo_markdown_files() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    std::fs::create_dir_all(repo_root.path().join("docs"))?;
    std::fs::write(
        repo_root.path().join("docs").join("PROJECT.md"),
        "# Project\n\nHTTP linked doc.\n",
    )?;
    std::fs::write(
        repo_root.path().join("docs").join("to-do.md"),
        "- [ ] Excluded from note picker\n",
    )?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "HTTP Linked Docs".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/projects/{}/notes/linkable-files", project.id))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body = to_bytes(list_response.into_body(), usize::MAX).await?;
    let list_json = String::from_utf8(list_body.to_vec())?;
    assert!(list_json.contains("PROJECT.md"));
    assert!(!list_json.contains("to-do.md"));

    let link_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/projects/{}/notes/link-file", project.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"relative_path":"docs/PROJECT.md"}"#.to_string(),
                ))?,
        )
        .await?;
    assert_eq!(link_response.status(), StatusCode::OK);

    let workspace = service.load_project_workspace(&project.id)?;
    assert_eq!(workspace.notes.len(), 1);
    assert_eq!(
        workspace.notes[0].sync_path.as_deref(),
        Some("docs/PROJECT.md")
    );

    let note = workspace.notes[0].clone();
    std::fs::write(
        repo_root.path().join("docs").join("PROJECT.md"),
        "# Project\n\nResolved via HTTP.\n",
    )?;

    let resolve_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/notes/{}/sync/resolve-file", note.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\"}}",
                    note.revision
                )))?,
        )
        .await?;
    assert_eq!(resolve_response.status(), StatusCode::OK);

    let refreshed = service.load_project_workspace(&project.id)?;
    assert!(refreshed.notes[0].body.contains("Resolved via HTTP."));

    let stale_resolve_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/notes/{}/sync/resolve-file", note.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\"}}",
                    note.revision
                )))?,
        )
        .await?;
    assert_eq!(stale_resolve_response.status(), StatusCode::CONFLICT);

    Ok(())
}

#[tokio::test]
async fn create_local_project_bootstraps_managed_todo_file() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    std::fs::create_dir_all(repo_root.path().join("docs"))?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/projects")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "mode": "local",
                        "source_locator": repo_root.path().to_string_lossy().to_string()
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(create_response.status(), StatusCode::OK);

    let projects = service.list_projects(20, false)?;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].source_kind, Some(ProjectSourceKind::Local));
    assert!(projects[0].task_sync_enabled);
    assert_eq!(
        projects[0].task_sync_mode,
        Some(TaskSyncMode::ManagedTodoFile)
    );
    assert_eq!(projects[0].task_sync_status, Some(TaskSyncStatus::Live));

    let managed_todo_path = repo_root.path().join("docs").join("to-do.md");
    assert!(managed_todo_path.is_file());
    let default_note_path = repo_root.path().join("docs").join("project-notes.md");
    assert!(default_note_path.is_file());

    let workspace = service.load_project_workspace(&projects[0].id)?;
    assert_eq!(workspace.notes.len(), 1);
    assert_eq!(
        workspace.notes[0].sync_kind,
        Some(topside::types::NoteSyncKind::RepoMarkdown)
    );
    assert_eq!(
        workspace.notes[0].sync_path.as_deref(),
        Some("docs/project-notes.md")
    );

    Ok(())
}

#[tokio::test]
async fn create_local_project_succeeds_when_bootstrap_fails() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    let repo_path = repo_root.path().to_path_buf();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&repo_path, std::fs::Permissions::from_mode(0o555))?;
    }

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/projects")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "mode": "local",
                        "source_locator": repo_path.to_string_lossy().to_string()
                    })
                    .to_string(),
                ))?,
        )
        .await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&repo_path, std::fs::Permissions::from_mode(0o755))?;
    }

    assert_eq!(create_response.status(), StatusCode::OK);

    let projects = service.list_projects(20, false)?;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].source_kind, Some(ProjectSourceKind::Local));
    let stored_locator = projects[0]
        .source_locator
        .as_deref()
        .expect("source locator should be set");
    assert_eq!(
        PathBuf::from(stored_locator).canonicalize()?,
        repo_path.canonicalize()?
    );

    #[cfg(unix)]
    {
        assert!(projects[0].task_sync_enabled);
        assert_eq!(projects[0].task_sync_status, Some(TaskSyncStatus::Paused));
        assert!(projects[0].task_sync_conflict_summary.is_some());
    }

    Ok(())
}

#[tokio::test]
async fn create_and_update_note_for_local_project_writes_docs_file() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Local Notes Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/notes")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "project_id": project.id,
                        "title": "Design Notes"
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(create_response.status(), StatusCode::OK);

    let workspace = service.load_project_workspace(&project.id)?;
    let note = workspace
        .notes
        .iter()
        .find(|item| item.sync_path.as_deref() == Some("docs/design-notes.md"))
        .expect("linked note should exist");
    assert_eq!(
        note.sync_kind,
        Some(topside::types::NoteSyncKind::RepoMarkdown)
    );
    let note_path = repo_root.path().join("docs").join("design-notes.md");
    assert!(note_path.is_file());

    let update_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/notes/{}", note.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "expected_revision": note.revision,
                        "body": "# Design Notes\n\nSaved through notes UI.\n"
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(update_response.status(), StatusCode::OK);

    let mut updated_on_disk = false;
    for _ in 0..40 {
        let contents = std::fs::read_to_string(&note_path).unwrap_or_default();
        if contents.contains("Saved through notes UI.") {
            updated_on_disk = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(updated_on_disk);

    Ok(())
}

#[tokio::test]
async fn dev_reload_token_endpoint_and_script_are_dev_only() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let dev_state = build_test_state(
        &service,
        Some("reload-7".to_string()),
        Arc::new(UnsupportedPortManager),
    )?;
    let dev_app = router(dev_state);

    let token_response = dev_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/__dev/reload-token")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(token_response.status(), StatusCode::OK);
    let token_body = to_bytes(token_response.into_body(), usize::MAX).await?;
    let token_json = String::from_utf8(token_body.to_vec())?;
    assert!(token_json.contains("reload-7"));

    let html_response = dev_app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty())?)
        .await?;
    assert_eq!(html_response.status(), StatusCode::OK);
    let html_body = to_bytes(html_response.into_body(), usize::MAX).await?;
    let html = String::from_utf8(html_body.to_vec())?;
    assert!(html.contains("/__dev/reload-token"));

    let non_dev_state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let non_dev_app = router(non_dev_state);

    let missing_response = non_dev_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/__dev/reload-token")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(missing_response.status(), StatusCode::NOT_FOUND);

    let non_dev_html_response = non_dev_app
        .oneshot(Request::builder().uri("/").body(Body::empty())?)
        .await?;
    assert_eq!(non_dev_html_response.status(), StatusCode::OK);
    let non_dev_html_body = to_bytes(non_dev_html_response.into_body(), usize::MAX).await?;
    let non_dev_html = String::from_utf8(non_dev_html_body.to_vec())?;
    assert!(!non_dev_html.contains("/__dev/reload-token"));

    Ok(())
}

#[tokio::test]
async fn bulk_archive_tasks_endpoint_archives_done_tasks() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Bulk Archive Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    service.create_task(
        CreateTaskPayload {
            title: "Done task one".to_string(),
            project_id: project.id.clone(),
            status: Some(TaskStatus::Done),
            priority: None,
            assignee: None,
            due_at: None,
            sort_order: None,
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;
    service.create_task(
        CreateTaskPayload {
            title: "Done task two".to_string(),
            project_id: project.id.clone(),
            status: Some(TaskStatus::Done),
            priority: None,
            assignee: None,
            due_at: None,
            sort_order: None,
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let done_tasks = service.list_tasks(&TaskFilters {
        status: Some(TaskStatus::Done),
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(10),
    })?;
    assert_eq!(done_tasks.len(), 2);

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let archive_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tasks/archive")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "project_id": project.id,
                        "tasks": done_tasks
                            .iter()
                            .map(|task| json!({
                                "id": task.id,
                                "expected_revision": task.revision,
                            }))
                            .collect::<Vec<_>>(),
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(archive_response.status(), StatusCode::OK);

    let remaining_done_tasks = service.list_tasks(&TaskFilters {
        status: Some(TaskStatus::Done),
        priority: None,
        project_id: Some(project.id),
        assignee: None,
        include_archived: false,
        limit: Some(10),
    })?;
    assert!(remaining_done_tasks.is_empty());

    Ok(())
}

#[tokio::test]
async fn project_http_mutations_cover_settings_actions() -> Result<()> {
    let (tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Settings Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let rename_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/projects/{}", project.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\",\"current_project_id\":\"{}\",\"title\":\"Renamed Project\",\"icon\":\"rocket\",\"source_kind\":\"local\",\"source_locator\":\"{}\"}}",
                    project.revision,
                    project.id,
                    tmp.path().display()
                )))?,
        )
        .await?;
    assert_eq!(rename_response.status(), StatusCode::OK);

    let projects = service.list_projects(10, false)?;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].title, "Renamed Project");
    assert_eq!(projects[0].icon.as_deref(), Some("rocket"));
    assert_eq!(
        projects[0].source_kind.as_ref().map(|kind| kind.as_str()),
        Some("local")
    );

    service.create_task(
        CreateTaskPayload {
            title: "Settings Task".to_string(),
            project_id: project.id.clone(),
            status: None,
            priority: None,
            assignee: None,
            due_at: None,
            sort_order: None,
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;
    service.create_note(
        CreateNotePayload {
            title: "Settings Note".to_string(),
            project_id: Some(project.id.clone()),
            tags: None,
            body: Some("archive alongside the project".to_string()),
        },
        Actor::human("tester"),
    )?;

    let archive_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/projects/{}/archive", project.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\",\"current_project_id\":null}}",
                    projects[0].revision
                )))?,
        )
        .await?;
    assert_eq!(archive_response.status(), StatusCode::OK);

    assert!(service.list_projects(10, false)?.is_empty());
    assert_eq!(service.list_projects(10, true)?.len(), 1);
    assert!(
        service
            .list_tasks(&TaskFilters {
                status: None,
                priority: None,
                project_id: Some(project.id.clone()),
                assignee: None,
                include_archived: false,
                limit: Some(10),
            })?
            .is_empty()
    );
    assert_eq!(
        service
            .list_tasks(&TaskFilters {
                status: None,
                priority: None,
                project_id: Some(project.id.clone()),
                assignee: None,
                include_archived: true,
                limit: Some(10),
            })?
            .len(),
        1
    );
    assert!(service.list_notes(10, false)?.is_empty());
    assert_eq!(service.list_notes(10, true)?.len(), 1);

    Ok(())
}

#[tokio::test]
async fn archive_menu_endpoints_list_restore_and_empty_archive() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Archive Menu Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;
    service.create_task(
        CreateTaskPayload {
            title: "Archive Menu Task".to_string(),
            project_id: project.id.clone(),
            status: None,
            priority: None,
            assignee: None,
            due_at: None,
            sort_order: None,
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;
    let note = service.create_note(
        CreateNotePayload {
            title: "Archive Menu Note".to_string(),
            project_id: Some(project.id.clone()),
            tags: None,
            body: Some("restore me".to_string()),
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let archive_project_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/projects/{}/archive", project.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\",\"current_project_id\":null}}",
                    project.revision
                )))?,
        )
        .await?;
    assert_eq!(archive_project_response.status(), StatusCode::OK);

    let archive_list_response = app
        .clone()
        .oneshot(Request::builder().uri("/api/archive").body(Body::empty())?)
        .await?;
    assert_eq!(archive_list_response.status(), StatusCode::OK);
    let archive_list_body = to_bytes(archive_list_response.into_body(), usize::MAX).await?;
    let archive_list_json: serde_json::Value = serde_json::from_slice(&archive_list_body)?;
    assert_eq!(archive_list_json["total_count"].as_u64(), Some(3));
    let listed_items = archive_list_json["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(listed_items.len(), 1);
    assert_eq!(
        listed_items[0]["title"].as_str(),
        Some("Archive Menu Project")
    );
    assert_eq!(
        listed_items[0]["detail_label"].as_str(),
        Some("1 task | 1 note")
    );

    let archived_project = service
        .list_projects(10, true)?
        .into_iter()
        .find(|item| item.archived)
        .expect("archived project");
    let restore_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/archive/{}/restore", archived_project.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\",\"current_project_id\":null}}",
                    archived_project.revision
                )))?,
        )
        .await?;
    assert_eq!(restore_response.status(), StatusCode::OK);

    assert_eq!(service.list_projects(10, false)?.len(), 1);
    assert_eq!(
        service
            .list_tasks(&TaskFilters {
                status: None,
                priority: None,
                project_id: Some(project.id.clone()),
                assignee: None,
                include_archived: false,
                limit: Some(10),
            })?
            .len(),
        1
    );
    assert_eq!(service.list_notes(10, false)?.len(), 1);

    let archive_note_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/notes/{}/archive", note.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\"}}",
                    service.load_project_workspace(&project.id)?.notes[0].revision
                )))?,
        )
        .await?;
    assert_eq!(archive_note_response.status(), StatusCode::OK);

    let archive_list_response = app
        .clone()
        .oneshot(Request::builder().uri("/api/archive").body(Body::empty())?)
        .await?;
    assert_eq!(archive_list_response.status(), StatusCode::OK);
    let archive_list_body = to_bytes(archive_list_response.into_body(), usize::MAX).await?;
    let archive_list_json: serde_json::Value = serde_json::from_slice(&archive_list_body)?;
    assert_eq!(archive_list_json["total_count"].as_u64(), Some(1));
    assert_eq!(
        archive_list_json["items"][0]["title"].as_str(),
        Some("Archive Menu Note")
    );

    let empty_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/archive/empty")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "current_project_id": project.id,
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(empty_response.status(), StatusCode::OK);
    assert_eq!(
        service
            .list_notes(10, true)?
            .into_iter()
            .filter(|item| item.archived)
            .count(),
        0
    );

    Ok(())
}

#[tokio::test]
async fn project_sync_endpoint_imports_repo_tasks() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    std::fs::write(
        repo_root.path().join("to-do.md"),
        "# Launch\n- [ ] Pick a name\n- [x] Ship alpha\n",
    )?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Sync HTTP Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let sync_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/projects/{}/sync", project.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"current_project_id\":\"{}\"}}",
                    project.id
                )))?,
        )
        .await?;
    assert_eq!(sync_response.status(), StatusCode::OK);

    let sync_body = to_bytes(sync_response.into_body(), usize::MAX).await?;
    let sync_json = String::from_utf8(sync_body.to_vec())?;
    assert!(sync_json.contains("Scanned 1 file(s), found 2 repo task(s)"));

    let tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(20),
    })?;
    assert_eq!(tasks.len(), 2);
    assert!(
        tasks
            .iter()
            .any(|task| task.title == "Ship alpha" && task.status == TaskStatus::Done)
    );

    Ok(())
}

#[tokio::test]
async fn codex_session_create_requires_desktop_header() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = TempDir::new()?;
    let project = service.create_project(
        CreateProjectPayload {
            title: "Codex Header Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/projects/{}/codex-sessions", project.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    Ok(())
}

#[tokio::test]
async fn codex_session_create_and_websocket_stream_work_with_mock_cli() -> Result<()> {
    let _env = MockCodexEnv::new()?;
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = TempDir::new()?;
    let project = service.create_project(
        CreateProjectPayload {
            title: "Codex Runtime Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let http_app = router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state))
            .await
            .expect("serve test router");
    });

    let create_response = http_app
        .oneshot(
            desktop_request(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/projects/{}/codex-sessions", project.id))
                    .header(header::CONTENT_TYPE, "application/json"),
            )
            .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(create_response.status(), StatusCode::OK);
    let create_body = to_bytes(create_response.into_body(), usize::MAX).await?;
    let create_json: serde_json::Value = serde_json::from_slice(&create_body)?;
    let opened_session_id = create_json["opened_session_id"]
        .as_str()
        .expect("opened session id")
        .to_string();

    let mut attached_codex_id = None;
    for _ in 0..80 {
        let workspace = service.load_project_workspace(&project.id)?;
        let session = workspace
            .codex_sessions
            .iter()
            .find(|session| session.id == opened_session_id)
            .cloned()
            .expect("session persisted");
        if session.status.as_str() == "live" && session.codex_session_id.is_some() {
            attached_codex_id = session.codex_session_id.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(
        attached_codex_id.is_some(),
        "expected reconciled codex session id"
    );

    let socket_url = format!(
        "ws://{}/api/codex-sessions/{}/pty?desktop=true",
        addr, opened_session_id
    );
    let (mut socket, _response) = connect_async(&socket_url).await?;

    let startup_output = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let Some(message) = socket.next().await else {
                anyhow::bail!("websocket closed before startup output")
            };
            match message? {
                Message::Text(text) => {
                    let payload: serde_json::Value = serde_json::from_str(text.as_ref())?;
                    if payload["type"] == "output" {
                        let data = payload["data"].as_str().unwrap_or_default().to_string();
                        if data.contains("mock-codex:") {
                            return Ok::<String, anyhow::Error>(data);
                        }
                    }
                }
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(_) => anyhow::bail!("websocket closed before startup output"),
                _ => {}
            }
        }
    })
    .await??;
    assert!(startup_output.contains("mock-codex:"));

    socket
        .send(Message::Text(
            r#"{"type":"input","data":"ping\r"}"#.to_string().into(),
        ))
        .await?;

    let echoed_output = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let Some(message) = socket.next().await else {
                anyhow::bail!("websocket closed before echo output")
            };
            match message? {
                Message::Text(text) => {
                    let payload: serde_json::Value = serde_json::from_str(text.as_ref())?;
                    if payload["type"] == "output" {
                        let data = payload["data"].as_str().unwrap_or_default().to_string();
                        if data.contains("ping") {
                            return Ok::<String, anyhow::Error>(data);
                        }
                    }
                }
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(_) => anyhow::bail!("websocket closed before echo output"),
                _ => {}
            }
        }
    })
    .await??;
    assert!(echoed_output.contains("ping"));

    let _ = socket.close(None).await;
    server.abort();

    Ok(())
}

#[tokio::test]
async fn codex_session_terminate_endpoint_marks_session_resumable() -> Result<()> {
    let _env = MockCodexEnv::new()?;
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = TempDir::new()?;
    let project = service.create_project(
        CreateProjectPayload {
            title: "Codex Terminate Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let create_response = app
        .clone()
        .oneshot(
            desktop_request(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/projects/{}/codex-sessions", project.id))
                    .header(header::CONTENT_TYPE, "application/json"),
            )
            .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(create_response.status(), StatusCode::OK);
    let create_body = to_bytes(create_response.into_body(), usize::MAX).await?;
    let create_json: serde_json::Value = serde_json::from_slice(&create_body)?;
    let session_id = create_json["opened_session_id"]
        .as_str()
        .expect("opened session id")
        .to_string();

    let created = wait_for_codex_session(&service, &project.id, &session_id, |session| {
        session.status.as_str() == "live" && session.codex_session_id.is_some()
    })
    .await?;
    assert!(created.codex_session_id.is_some());

    let terminate_response = app
        .clone()
        .oneshot(
            desktop_request(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/codex-sessions/{session_id}/terminate")),
            )
            .body(Body::empty())?,
        )
        .await?;
    assert_eq!(terminate_response.status(), StatusCode::OK);
    let terminate_body = to_bytes(terminate_response.into_body(), usize::MAX).await?;
    let terminate_json: serde_json::Value = serde_json::from_slice(&terminate_body)?;
    assert_eq!(terminate_json["message"], json!("Codex session ended"));
    assert_eq!(
        terminate_json["workspace"]["codex_sessions"]
            .as_array()
            .map(|sessions| sessions.len()),
        Some(1)
    );

    let terminated = wait_for_codex_session(&service, &project.id, &session_id, |session| {
        session.status.as_str() == "resumable" && session.ended_at.is_some()
    })
    .await?;
    assert_eq!(terminated.id, session_id);
    assert!(terminated.ended_at.is_some());

    Ok(())
}

#[tokio::test]
async fn codex_session_archive_endpoint_removes_session_and_selects_fallback() -> Result<()> {
    let _env = MockCodexEnv::new()?;
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = TempDir::new()?;
    let project = service.create_project(
        CreateProjectPayload {
            title: "Codex Archive Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            icon: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = build_test_state(&service, None, Arc::new(UnsupportedPortManager))?;
    let app = router(state);

    let first_create_response = app
        .clone()
        .oneshot(
            desktop_request(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/projects/{}/codex-sessions", project.id))
                    .header(header::CONTENT_TYPE, "application/json"),
            )
            .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(first_create_response.status(), StatusCode::OK);
    let first_create_body = to_bytes(first_create_response.into_body(), usize::MAX).await?;
    let first_create_json: serde_json::Value = serde_json::from_slice(&first_create_body)?;
    let first_session_id = first_create_json["opened_session_id"]
        .as_str()
        .expect("first opened session id")
        .to_string();

    let second_create_response = app
        .clone()
        .oneshot(
            desktop_request(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/projects/{}/codex-sessions", project.id))
                    .header(header::CONTENT_TYPE, "application/json"),
            )
            .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(second_create_response.status(), StatusCode::OK);
    let second_create_body = to_bytes(second_create_response.into_body(), usize::MAX).await?;
    let second_create_json: serde_json::Value = serde_json::from_slice(&second_create_body)?;
    let second_session_id = second_create_json["opened_session_id"]
        .as_str()
        .expect("second opened session id")
        .to_string();

    wait_for_codex_session(&service, &project.id, &second_session_id, |session| {
        session.status.as_str() == "live" || session.status.as_str() == "launching"
    })
    .await?;

    let archive_response = app
        .clone()
        .oneshot(
            desktop_request(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/codex-sessions/{second_session_id}/archive")),
            )
            .body(Body::empty())?,
        )
        .await?;
    assert_eq!(archive_response.status(), StatusCode::OK);
    let archive_body = to_bytes(archive_response.into_body(), usize::MAX).await?;
    let archive_json: serde_json::Value = serde_json::from_slice(&archive_body)?;
    assert_eq!(archive_json["message"], json!("Codex session archived"));
    assert_eq!(archive_json["opened_session_id"], json!(first_session_id));

    let response_sessions = archive_json["workspace"]["codex_sessions"]
        .as_array()
        .expect("workspace codex sessions");
    assert_eq!(response_sessions.len(), 1);
    assert_eq!(response_sessions[0]["id"], json!(first_session_id));

    wait_for_codex_session_absent(&service, &project.id, &second_session_id).await?;
    assert!(service.get_codex_session(&second_session_id)?.is_none());
    assert_eq!(
        service
            .load_project_workspace(&project.id)?
            .codex_sessions
            .len(),
        1
    );

    Ok(())
}

#[tokio::test]
async fn system_ports_endpoint_serializes_live_port_rows() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let port_manager = FakePortManager::new(
        Ok(vec![PortSession {
            pid: 999,
            port: 3000,
            process_name: "node".to_string(),
            command_line: "/usr/local/bin/node server.js".to_string(),
            user: "anthonymarti".to_string(),
            bindings: vec!["127.0.0.1:3000".to_string(), "[::1]:3000".to_string()],
            other_ports: vec![3001],
            is_topside_process: false,
            can_terminate: true,
            is_likely_dev: true,
        }]),
        Ok(TerminatePortResult {
            items: Vec::new(),
            message: String::new(),
        }),
    );

    let state = build_test_state(&service, None, Arc::new(port_manager.clone()))?;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/system/ports")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let payload: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(payload["items"][0]["pid"], json!(999));
    assert_eq!(payload["items"][0]["port"], json!(3000));
    assert_eq!(payload["items"][0]["process_name"], json!("node"));
    assert_eq!(
        payload["items"][0]["bindings"],
        json!(["127.0.0.1:3000", "[::1]:3000"])
    );
    assert_eq!(payload["items"][0]["other_ports"], json!([3001]));
    assert_eq!(payload["items"][0]["can_terminate"], json!(true));
    assert_eq!(payload["items"][0]["is_likely_dev"], json!(true));

    let (list_calls, terminate_calls) = port_manager.snapshot();
    assert_eq!(list_calls, 1);
    assert!(terminate_calls.is_empty());

    Ok(())
}

#[tokio::test]
async fn system_port_terminate_endpoint_returns_refreshed_items_and_message() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let port_manager = FakePortManager::new(
        Ok(Vec::new()),
        Ok(TerminatePortResult {
            items: vec![PortSession {
                pid: 998,
                port: 4000,
                process_name: "api".to_string(),
                command_line: "/usr/local/bin/api --watch".to_string(),
                user: "anthonymarti".to_string(),
                bindings: vec!["*:4000".to_string()],
                other_ports: Vec::new(),
                is_topside_process: false,
                can_terminate: true,
                is_likely_dev: false,
            }],
            message: "Force ended session on port 3000".to_string(),
        }),
    );

    let state = build_test_state(&service, None, Arc::new(port_manager.clone()))?;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/system/ports/terminate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"pid":999,"port":3000}"#))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let payload: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        payload["message"],
        json!("Force ended session on port 3000")
    );
    assert_eq!(payload["items"][0]["port"], json!(4000));

    let (list_calls, terminate_calls) = port_manager.snapshot();
    assert_eq!(list_calls, 0);
    assert_eq!(terminate_calls, vec![(999, 3000)]);

    Ok(())
}

#[tokio::test]
async fn system_port_terminate_endpoint_maps_forbidden_errors() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let port_manager = FakePortManager::new(
        Ok(Vec::new()),
        Err(PortManagerError::Forbidden(
            "Topside cannot terminate its own process".to_string(),
        )),
    );

    let state = build_test_state(&service, None, Arc::new(port_manager.clone()))?;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/system/ports/terminate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"pid":10158,"port":7410}"#))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let payload: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        payload["error"],
        json!("Topside cannot terminate its own process")
    );

    let (_list_calls, terminate_calls) = port_manager.snapshot();
    assert_eq!(terminate_calls, vec![(10158, 7410)]);

    Ok(())
}
