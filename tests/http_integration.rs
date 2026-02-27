mod common;

use std::sync::Arc;

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::util::ServiceExt;

use n10e::http::{WebState, router};
use n10e::types::{Actor, CreateProjectPayload};

#[tokio::test]
async fn dashboard_and_partial_endpoints_work_with_etags() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let _project = service.create_project(
        CreateProjectPayload {
            title: "HTTP Project".to_string(),
            owner: None,
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
    assert!(html.contains("local-first agent workspace"));

    let response_1 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/partials/tasks")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response_1.status(), StatusCode::OK);
    let etag = response_1
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(!etag.is_empty());

    let response_2 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/partials/tasks")
                .header(header::IF_NONE_MATCH, etag)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response_2.status(), StatusCode::NOT_MODIFIED);

    Ok(())
}

#[tokio::test]
async fn task_http_mutations_and_conflict_path() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Mutation Project".to_string(),
            owner: None,
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

    let create_form = format!(
        "title={}&project_id={}&status=todo&priority=P2&assignee=agent%3Acodex",
        urlencoding::encode("HTTP Task"),
        urlencoding::encode(&project.id)
    );

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(create_form))?,
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

    let update_form = format!(
        "expected_revision={}&status=done&priority=P1&assignee=agent%3Acodex",
        urlencoding::encode(&task.revision)
    );

    let update_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/tasks/{}", task.id))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(update_form))?,
        )
        .await?;
    assert_eq!(update_response.status(), StatusCode::OK);

    let stale_response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/tasks/{}", task.id))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "expected_revision={}&status=in_progress",
                    urlencoding::encode(&task.revision)
                )))?,
        )
        .await?;

    assert_eq!(stale_response.status(), StatusCode::CONFLICT);
    Ok(())
}
