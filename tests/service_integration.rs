mod common;

use anyhow::Result;
use rusqlite::params;

use n10e::db::Db;
use n10e::service::ServiceError;
use n10e::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, SearchFilters, TaskFilters,
    TaskPatch,
};

#[test]
fn service_crud_conflict_archive_and_backlinks() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Alpha Project".to_string(),
            owner: Some("human:anthony".to_string()),
            tags: Some(vec!["alpha".to_string()]),
            body: Some("Project bootstrap".to_string()),
        },
        Actor::human("tester"),
    )?;

    let note = service.create_note(
        CreateNotePayload {
            title: "Alpha Note".to_string(),
            project_id: Some(project.id.clone()),
            tags: None,
            body: Some(format!(
                "Reference project [[project:{}]] and future task links.",
                project.id
            )),
        },
        Actor::human("tester"),
    )?;

    let task = service.create_task(
        CreateTaskPayload {
            title: "Implement Core Flow".to_string(),
            project_id: project.id.clone(),
            status: None,
            priority: None,
            assignee: Some("agent:codex".to_string()),
            due_at: None,
            tags: Some(vec!["integration".to_string()]),
            body: Some(format!("Task references note [[note:{}]].", note.id)),
        },
        Actor::human("tester"),
    )?;

    let conn = rusqlite::Connection::open(service.config.db_path())?;
    let link_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entity_links WHERE source_id = ?1",
        params![task.id],
        |row| row.get(0),
    )?;
    assert_eq!(link_count, 1);

    let stale_update = service.update_task(
        &task.id,
        TaskPatch {
            title: Some("Should conflict".to_string()),
            ..Default::default()
        },
        "stale-revision",
        Actor::human("tester"),
    );

    assert!(matches!(
        stale_update,
        Err(ServiceError::Conflict {
            expected: _,
            current: _
        })
    ));

    let updated = service.update_task(
        &task.id,
        TaskPatch {
            title: Some("Implement Core Flow (updated)".to_string()),
            status: Some(n10e::types::TaskStatus::InProgress),
            ..Default::default()
        },
        &task.revision,
        Actor::human("tester"),
    )?;

    assert_ne!(task.revision, updated.revision);

    let archived = service.archive_entity(&task.id, &updated.revision, Actor::human("tester"))?;
    assert!(archived.archived);

    let active_tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: None,
        assignee: None,
        include_archived: false,
        limit: Some(100),
    })?;
    assert!(active_tasks.iter().all(|t| t.id != task.id));

    let all_tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: None,
        assignee: None,
        include_archived: true,
        limit: Some(100),
    })?;
    assert!(all_tasks.iter().any(|t| t.id == task.id && t.archived));

    let search = service.search_context(
        "benchmark",
        &SearchFilters {
            entity_type: None,
            project_id: None,
            include_archived: true,
        },
        Some(20),
    )?;
    assert!(search.is_empty());

    let activity = service.list_recent_activity(None, 100)?;
    assert!(activity.len() >= 5);

    Ok(())
}

#[test]
fn reindex_skips_malformed_frontmatter_but_keeps_valid_files() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Valid Project".to_string(),
            owner: None,
            tags: None,
            body: Some("valid".to_string()),
        },
        Actor::human("tester"),
    )?;

    let malformed = service.config.notes_dir().join("broken.md");
    std::fs::write(
        &malformed,
        "---\nid: nte_bad\ntype: note\ntitle broken\n---\nbody",
    )?;

    service.reindex_all()?;

    let loaded = service.read_entity(&project.id)?;
    assert!(loaded.is_some());

    Ok(())
}

#[test]
fn migrations_are_forward_only_and_idempotent() -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let config = common::prepare_workspace_config(tmp.path())?;
    let db = Db::open(&config.db_path())?;

    db.run_migrations()?;
    db.run_migrations()?;

    let conn = rusqlite::Connection::open(config.db_path())?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
        row.get(0)
    })?;
    assert_eq!(count, 1);

    Ok(())
}
