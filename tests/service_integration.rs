mod common;

use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use rusqlite::params;

use n10e::db::Db;
use n10e::service::ServiceError;
use n10e::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, ProjectSourceKind,
    SearchFilters, TaskFilters, TaskPatch, TaskStatus, TaskSyncKind, TaskSyncStatus,
};

fn wait_for(label: &str, mut predicate: impl FnMut() -> Result<bool>) -> Result<()> {
    let started = Instant::now();
    loop {
        if predicate()? {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(5) {
            anyhow::bail!("timed out waiting for condition: {label}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn project_by_id(service: &n10e::service::AppService, project_id: &str) -> Result<n10e::types::ProjectItem> {
    service
        .list_projects(20, false)?
        .into_iter()
        .find(|item| item.id == project_id)
        .ok_or_else(|| anyhow::anyhow!("project {project_id} not found"))
}

#[test]
fn service_crud_conflict_archive_and_backlinks() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Alpha Project".to_string(),
            owner: Some("human:anthony".to_string()),
            source_kind: None,
            source_locator: None,
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
            sort_order: None,
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: None,
            tags: Some(vec!["integration".to_string()]),
            body: Some(format!("Task references note [[note:{}]].", note.id)),
        },
        Actor::human("tester"),
    )?;

    let workspace = service.load_project_workspace(&project.id)?;
    assert_eq!(workspace.notes.len(), 1);
    assert!(workspace.notes[0].body.contains("Reference project"));

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
            source_kind: None,
            source_locator: None,
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
    assert_eq!(count, 3);

    Ok(())
}

#[test]
fn sync_project_imports_and_updates_repo_todo_tasks() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    std::fs::write(
        repo_root.path().join("to-do.md"),
        "# Launch\n- [ ] Pick a name\n- [x] Ship alpha\n",
    )?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Synced Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let first_report = service.sync_project_from_source(&project.id, Actor::human("tester"))?;
    assert_eq!(first_report.files_scanned, 1);
    assert_eq!(first_report.tasks_found, 2);
    assert_eq!(first_report.created, 2);
    assert_eq!(first_report.updated, 0);

    let first_tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(20),
    })?;
    assert_eq!(first_tasks.len(), 2);
    assert!(first_tasks.iter().all(|task| task.sync_managed));
    assert!(first_tasks.iter().all(|task| {
        task.sync_kind == Some(TaskSyncKind::RepoMarkdown)
            && task.sync_path.as_deref() == Some("to-do.md")
    }));

    std::fs::write(
        repo_root.path().join("to-do.md"),
        "# Launch\n- [x] Pick a product name\n- [x] Ship alpha\n- [ ] Add analytics\n",
    )?;

    let second_report = service.sync_project_from_source(&project.id, Actor::human("tester"))?;
    assert_eq!(second_report.created, 1);
    assert_eq!(second_report.updated, 1);

    let second_tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(20),
    })?;
    assert_eq!(second_tasks.len(), 3);
    assert!(
        second_tasks
            .iter()
            .any(|task| task.title == "Pick a product name" && task.status == TaskStatus::Done)
    );
    assert!(
        second_tasks
            .iter()
            .any(|task| task.title == "Add analytics" && task.status == TaskStatus::Todo)
    );

    let synced_project = service
        .list_projects(10, false)?
        .into_iter()
        .find(|item| item.id == project.id)
        .expect("project should still exist");
    assert!(synced_project.last_synced_at.is_some());
    assert!(
        synced_project
            .last_sync_summary
            .as_deref()
            .unwrap_or_default()
            .contains("Scanned 1 file(s), found 3 repo task(s)")
    );

    Ok(())
}

#[test]
fn managed_task_sync_handles_outbound_inbound_conflict_and_recovery() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    let sync_path = repo_root.path().join("docs/to-do.md");
    let sidecar_path = repo_root.path().join("docs/.to-do.n10e-sync.json");

    let project = service.create_project(
        CreateProjectPayload {
            title: "Managed Sync Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let _task = service.create_task(
        CreateTaskPayload {
            title: "Draft spec".to_string(),
            project_id: project.id.clone(),
            status: Some(TaskStatus::Todo),
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

    let enabled = service.enable_managed_task_sync(&project.id, &project.revision)?;
    assert_eq!(enabled.task_sync_status, Some(TaskSyncStatus::Live));
    assert_eq!(enabled.task_sync_file.as_deref(), Some("docs/to-do.md"));
    assert!(sync_path.exists());
    assert!(sidecar_path.exists());

    let initial_file = std::fs::read_to_string(&sync_path)?;
    assert!(initial_file.contains("- [ ] Draft spec"));
    assert!(!initial_file.contains("n10e:id="));

    let live_task = service
        .list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project.id.clone()),
            assignee: None,
            include_archived: false,
            limit: Some(20),
        })?
        .into_iter()
        .find(|item| item.title == "Draft spec")
        .expect("managed task should exist after enable");

    let _updated = service.update_task(
        &live_task.id,
        TaskPatch {
            title: Some("Draft API spec".to_string()),
            ..Default::default()
        },
        &live_task.revision,
        Actor::human("tester"),
    )?;

    wait_for("outbound write after local update", || {
        Ok(std::fs::read_to_string(&sync_path)?.contains("Draft API spec"))
    })?;

    let current_file = std::fs::read_to_string(&sync_path)?;
    std::fs::write(&sync_path, format!("{current_file}- [ ] External item\n"))?;
    let project_after_external_edit = project_by_id(&service, &project.id)?;
    let resolved_after_external_edit = service.resolve_managed_task_sync_from_file(
        &project.id,
        &project_after_external_edit.revision,
    )?;
    assert_eq!(resolved_after_external_edit.task_sync_status, Some(TaskSyncStatus::Live));
    assert!(
        service
            .list_tasks(&TaskFilters {
                status: None,
                priority: None,
                project_id: Some(project.id.clone()),
                assignee: None,
                include_archived: false,
                limit: Some(20),
            })?
            .iter()
            .any(|item| item.title == "External item")
    );

    std::fs::write(&sidecar_path, "{not-json")?;
    let current_file = std::fs::read_to_string(&sync_path)?;
    std::fs::write(&sync_path, format!("{current_file}- [ ] Recovered item\n"))?;
    let project_after_sidecar_break = project_by_id(&service, &project.id)?;
    let resolved_after_sidecar_break = service.resolve_managed_task_sync_from_file(
        &project.id,
        &project_after_sidecar_break.revision,
    )?;
    assert_eq!(resolved_after_sidecar_break.task_sync_status, Some(TaskSyncStatus::Live));
    assert!(
        service
            .list_tasks(&TaskFilters {
                status: None,
                priority: None,
                project_id: Some(project.id.clone()),
                assignee: None,
                include_archived: false,
                limit: Some(20),
            })?
            .iter()
            .any(|item| item.title == "Recovered item")
    );
    wait_for("sidecar rewrite after malformed sidecar", || {
        Ok(std::fs::read_to_string(&sidecar_path)?.contains("Recovered item"))
    })?;

    let conflict_task = service
        .list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project.id.clone()),
            assignee: None,
            include_archived: false,
            limit: Some(20),
        })?
        .into_iter()
        .find(|item| item.title == "Draft API spec")
        .expect("synced task should still exist");

    let _pending_conflict = service.update_task(
        &conflict_task.id,
        TaskPatch {
            title: Some("Local pending conflict".to_string()),
            ..Default::default()
        },
        &conflict_task.revision,
        Actor::human("tester"),
    )?;

    thread::sleep(Duration::from_millis(50));
    let current_file = std::fs::read_to_string(&sync_path)?;
    std::fs::write(&sync_path, format!("{current_file}- [ ] File wins task\n"))?;

    wait_for("conflict detection", || {
        let project = project_by_id(&service, &project.id)?;
        Ok(project.task_sync_status == Some(TaskSyncStatus::Conflict))
    })?;

    let conflicted = project_by_id(&service, &project.id)?;
    assert!(
        conflicted
            .task_sync_conflict_summary
            .as_deref()
            .unwrap_or_default()
            .contains("Managed task sync detected")
    );

    let resolved = service.resolve_managed_task_sync_from_file(&project.id, &conflicted.revision)?;
    assert_eq!(resolved.task_sync_status, Some(TaskSyncStatus::Live));

    wait_for("resolve from file imports winning task", || {
        let tasks = service.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project.id.clone()),
            assignee: None,
            include_archived: false,
            limit: Some(30),
        })?;
        Ok(tasks.iter().any(|item| item.title == "File wins task"))
    })?;

    let final_project = project_by_id(&service, &project.id)?;
    assert_eq!(final_project.task_sync_status, Some(TaskSyncStatus::Live));
    assert!(final_project.task_sync_conflict_summary.is_none());
    assert!(final_project.task_sync_last_inbound_at.is_some());
    assert!(std::fs::read_to_string(&sidecar_path)?.contains("File wins task"));

    Ok(())
}

#[test]
#[ignore = "profiling harness; run with -- --ignored --nocapture"]
fn sync_project_from_source_profile_first_and_mixed() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    let todo_path = repo_root.path().join("to-do.md");

    let initial_task_count = 40usize;
    let updated_task_count = 10usize;
    let new_task_count = 10usize;

    let mut initial_body = String::from("# Launch\n");
    for index in 0..initial_task_count {
        initial_body.push_str(&format!("- [ ] Initial task {}\n", index + 1));
    }
    std::fs::write(&todo_path, initial_body)?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Profiled Sync Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let first_started = Instant::now();
    let first_report = service.sync_project_from_source(&project.id, Actor::human("tester"))?;
    let first_elapsed = first_started.elapsed();
    assert_eq!(first_report.created, initial_task_count);
    assert_eq!(first_report.updated, 0);

    let mut mixed_body = String::from("# Launch\n");
    for index in 0..initial_task_count {
        if index < updated_task_count {
            mixed_body.push_str(&format!("- [x] Renamed task {}\n", index + 1));
        } else {
            mixed_body.push_str(&format!("- [ ] Initial task {}\n", index + 1));
        }
    }
    for index in 0..new_task_count {
        mixed_body.push_str(&format!("- [ ] Added task {}\n", index + 1));
    }
    std::fs::write(&todo_path, mixed_body)?;

    let mixed_started = Instant::now();
    let mixed_report = service.sync_project_from_source(&project.id, Actor::human("tester"))?;
    let mixed_elapsed = mixed_started.elapsed();
    assert_eq!(mixed_report.created, new_task_count);
    assert_eq!(mixed_report.updated, updated_task_count);

    let tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(100),
    })?;
    assert_eq!(tasks.len(), initial_task_count + new_task_count);

    println!(
        "sync_profile::initial_tasks={} updated_tasks={} new_tasks={} first_sync_ms={:.3} mixed_sync_ms={:.3}",
        initial_task_count,
        updated_task_count,
        new_task_count,
        first_elapsed.as_secs_f64() * 1000.0,
        mixed_elapsed.as_secs_f64() * 1000.0,
    );

    Ok(())
}
