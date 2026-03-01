mod common;

use std::sync::Arc;

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::util::ServiceExt;

use n10e::http::{WebState, router};
use n10e::types::{Actor, CreateProjectPayload, ProjectSourceKind, TaskFilters, TaskStatus};

#[tokio::test]
async fn dashboard_and_api_workspace_endpoints_work() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let _project = service.create_project(
        CreateProjectPayload {
            title: "HTTP Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            tags: None,
            body: Some("http body".to_string()),
        },
        Actor::human("tester"),
    )?;

    let state = Arc::new(WebState {
        service: Arc::new(service),
        dev_reload_token: None,
    });
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
async fn task_http_mutations_and_conflict_path() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Mutation Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = Arc::new(WebState {
        service: Arc::new(service.clone()),
        dev_reload_token: None,
    });
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

    let tasks = service.list_tasks(&n10e::types::TaskFilters {
        status: None,
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(10),
    })?;
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];

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
async fn dev_reload_token_endpoint_and_script_are_dev_only() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let dev_state = Arc::new(WebState {
        service: Arc::new(service.clone()),
        dev_reload_token: Some("reload-7".to_string()),
    });
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

    let non_dev_state = Arc::new(WebState {
        service: Arc::new(service),
        dev_reload_token: None,
    });
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
async fn project_http_mutations_cover_settings_actions() -> Result<()> {
    let (tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Settings Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = Arc::new(WebState {
        service: Arc::new(service.clone()),
        dev_reload_token: None,
    });
    let app = router(state);

    let rename_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/projects/{}", project.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"expected_revision\":\"{}\",\"current_project_id\":\"{}\",\"title\":\"Renamed Project\",\"source_kind\":\"local\",\"source_locator\":\"{}\"}}",
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
    assert_eq!(
        projects[0].source_kind.as_ref().map(|kind| kind.as_str()),
        Some("local")
    );

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
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let state = Arc::new(WebState {
        service: Arc::new(service.clone()),
        dev_reload_token: None,
    });
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
