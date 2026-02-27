mod common;

use std::sync::Arc;

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::util::ServiceExt;

use n10e::http::{WebState, router};
use n10e::types::{Actor, CreateProjectPayload};

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
        poll: service.config.poll.clone(),
        service: Arc::new(service),
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
        poll: service.config.poll.clone(),
        service: Arc::new(service.clone()),
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
    Ok(())
}
