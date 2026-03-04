mod common;

use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use rusqlite::params;

use topside::db::Db;
use topside::service::ServiceError;
use topside::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, NotePatch, NoteSyncKind,
    NoteSyncStatus, ProjectPatch, ProjectSourceKind, SearchFilters, TaskFilters, TaskPatch,
    TaskStatus, TaskSyncKind, TaskSyncStatus,
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

fn project_by_id(
    service: &topside::service::AppService,
    project_id: &str,
) -> Result<topside::types::ProjectItem> {
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
            status: Some(topside::types::TaskStatus::InProgress),
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
fn reopening_done_task_restores_nearest_done_heading_to_todo() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Reopen Heading Project".to_string(),
            owner: None,
            source_kind: None,
            source_locator: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let outer_heading = service.create_task(
        CreateTaskPayload {
            title: "## Outer Section".to_string(),
            project_id: project.id.clone(),
            status: Some(TaskStatus::Done),
            priority: None,
            assignee: None,
            due_at: None,
            sort_order: Some(1),
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;
    let _outer_task = service.create_task(
        CreateTaskPayload {
            title: "Outer completed task".to_string(),
            project_id: project.id.clone(),
            status: Some(TaskStatus::Done),
            priority: None,
            assignee: None,
            due_at: None,
            sort_order: Some(2),
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;
    let inner_heading = service.create_task(
        CreateTaskPayload {
            title: "## Inner Section".to_string(),
            project_id: project.id.clone(),
            status: Some(TaskStatus::Done),
            priority: None,
            assignee: None,
            due_at: None,
            sort_order: Some(3),
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;
    let inner_task = service.create_task(
        CreateTaskPayload {
            title: "Inner completed task".to_string(),
            project_id: project.id.clone(),
            status: Some(TaskStatus::Done),
            priority: None,
            assignee: None,
            due_at: None,
            sort_order: Some(4),
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: None,
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let _updated = service.update_task(
        &inner_task.id,
        TaskPatch {
            status: Some(TaskStatus::Todo),
            ..Default::default()
        },
        &inner_task.revision,
        Actor::human("tester"),
    )?;

    let tasks = service.list_tasks(&TaskFilters {
        status: None,
        priority: None,
        project_id: Some(project.id.clone()),
        assignee: None,
        include_archived: false,
        limit: Some(20),
    })?;

    let outer_heading = tasks
        .iter()
        .find(|task| task.id == outer_heading.id)
        .expect("outer heading present");
    let inner_heading = tasks
        .iter()
        .find(|task| task.id == inner_heading.id)
        .expect("inner heading present");
    let inner_task = tasks
        .iter()
        .find(|task| task.id == inner_task.id)
        .expect("inner task present");

    assert_eq!(outer_heading.status, TaskStatus::Done);
    assert_eq!(inner_heading.status, TaskStatus::Todo);
    assert_eq!(inner_task.status, TaskStatus::Todo);

    let workspace = service.load_project_workspace(&project.id)?;
    assert!(
        workspace
            .active_tasks
            .iter()
            .any(|task| task.id == inner_heading.id)
    );
    assert!(
        workspace
            .active_tasks
            .iter()
            .any(|task| task.id == inner_task.id)
    );

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
    assert_eq!(count, 5);

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
    let sidecar_path = repo_root.path().join("docs/.to-do.topside-sync.json");
    let legacy_sidecar_path = repo_root.path().join("docs/.to-do.n10e-sync.json");

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

    let initial_sidecar = std::fs::read_to_string(&sidecar_path)?;
    std::fs::remove_file(&sidecar_path)?;
    std::fs::write(&legacy_sidecar_path, initial_sidecar)?;

    let project_after_legacy_sidecar = project_by_id(&service, &project.id)?;
    let resolved_after_legacy_sidecar = service
        .resolve_managed_task_sync_from_file(&project.id, &project_after_legacy_sidecar.revision)?;
    assert_eq!(
        resolved_after_legacy_sidecar.task_sync_status,
        Some(TaskSyncStatus::Live)
    );

    wait_for("legacy sidecar rewrite", || {
        Ok(sidecar_path.exists() && !legacy_sidecar_path.exists())
    })?;

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
    let resolved_after_external_edit = service
        .resolve_managed_task_sync_from_file(&project.id, &project_after_external_edit.revision)?;
    assert_eq!(
        resolved_after_external_edit.task_sync_status,
        Some(TaskSyncStatus::Live)
    );
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
    let resolved_after_sidecar_break = service
        .resolve_managed_task_sync_from_file(&project.id, &project_after_sidecar_break.revision)?;
    assert_eq!(
        resolved_after_sidecar_break.task_sync_status,
        Some(TaskSyncStatus::Live)
    );
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

    let resolved =
        service.resolve_managed_task_sync_from_file(&project.id, &conflicted.revision)?;
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
fn managed_task_sync_resolve_from_local_rewrites_file() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    let sync_path = repo_root.path().join("docs/to-do.md");

    let project = service.create_project(
        CreateProjectPayload {
            title: "Managed Local Resolve Project".to_string(),
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
            title: "Local source of truth".to_string(),
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
        .find(|item| item.title == "Local source of truth")
        .expect("managed task should exist after enable");

    let _updated = service.update_task(
        &live_task.id,
        TaskPatch {
            title: Some("Local state wins".to_string()),
            ..Default::default()
        },
        &live_task.revision,
        Actor::human("tester"),
    )?;

    thread::sleep(Duration::from_millis(50));
    let current_file = std::fs::read_to_string(&sync_path)?;
    std::fs::write(
        &sync_path,
        format!("{current_file}- [ ] External stray line\n"),
    )?;

    wait_for("hash mismatch conflict before local resolve", || {
        let project = project_by_id(&service, &project.id)?;
        Ok(project.task_sync_status == Some(TaskSyncStatus::Conflict))
    })?;

    let conflicted = project_by_id(&service, &project.id)?;
    let resolved =
        service.resolve_managed_task_sync_from_local(&project.id, &conflicted.revision)?;
    assert_eq!(resolved.task_sync_status, Some(TaskSyncStatus::Live));

    let resolved_file = std::fs::read_to_string(&sync_path)?;
    assert!(resolved_file.contains("- [ ] Local state wins"));
    assert!(!resolved_file.contains("External stray line"));

    let recent = service.list_recent_activity(None, 20)?;
    assert!(
        recent
            .iter()
            .any(|item| item.action == "resolve_task_sync_from_local")
    );

    Ok(())
}

#[test]
fn linked_note_sync_links_files_and_reconciles_conflicts() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    let doc_path = repo_root.path().join("docs/ARCHITECTURE.md");
    std::fs::create_dir_all(doc_path.parent().expect("doc parent exists"))?;
    std::fs::write(&doc_path, "# Architecture\n\nInitial draft.\n")?;
    std::fs::write(
        repo_root.path().join("docs/to-do.md"),
        "- [ ] Keep task sync separate\n",
    )?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Linked Docs Project".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let files = service.list_linkable_note_files(&project.id)?;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative_path, "docs/ARCHITECTURE.md");

    let linked = service.link_note_to_repo_file(
        &project.id,
        "docs/ARCHITECTURE.md",
        Actor::human("tester"),
    )?;
    let linked_again = service.link_note_to_repo_file(
        &project.id,
        "docs/ARCHITECTURE.md",
        Actor::human("tester"),
    )?;
    assert_eq!(linked.id, linked_again.id);

    let workspace = service.load_project_workspace(&project.id)?;
    assert_eq!(workspace.notes.len(), 1);
    assert_eq!(
        workspace.notes[0].sync_kind,
        Some(NoteSyncKind::RepoMarkdown)
    );
    assert_eq!(workspace.notes[0].sync_status, Some(NoteSyncStatus::Live));
    assert_eq!(
        workspace.notes[0].sync_path.as_deref(),
        Some("docs/ARCHITECTURE.md")
    );
    assert!(workspace.notes[0].body.contains("Initial draft."));

    let _updated = service.update_note(
        &linked.id,
        NotePatch {
            title: Some("Should stay file-owned".to_string()),
            body: Some("# Architecture\n\nLocal revision.\n".to_string()),
            ..Default::default()
        },
        &linked.revision,
        Actor::human("tester"),
    )?;

    wait_for("outbound linked note write", || {
        Ok(std::fs::read_to_string(&doc_path)?.contains("Local revision."))
    })?;

    std::fs::write(&doc_path, "# Architecture\n\nExternal revision.\n")?;
    let imported = service.resolve_note_sync_from_file(&linked.id)?;
    assert!(imported.body.contains("External revision."));

    let note_after_import = service
        .load_project_workspace(&project.id)?
        .notes
        .into_iter()
        .find(|item| item.id == linked.id)
        .expect("linked note should exist");

    let _pending_conflict = service.update_note(
        &linked.id,
        NotePatch {
            body: Some("# Architecture\n\nLocal conflict draft.\n".to_string()),
            ..Default::default()
        },
        &note_after_import.revision,
        Actor::human("tester"),
    )?;

    thread::sleep(Duration::from_millis(50));
    std::fs::write(&doc_path, "# Architecture\n\nFile wins.\n")?;

    wait_for("linked note conflict detection", || {
        let note = service
            .load_project_workspace(&project.id)?
            .notes
            .into_iter()
            .find(|item| item.id == linked.id)
            .expect("linked note should exist");
        Ok(note.sync_status == Some(NoteSyncStatus::Conflict))
    })?;

    let resolved_from_file = service.resolve_note_sync_from_file(&linked.id)?;
    match resolved_from_file.frontmatter {
        topside::types::EntityFrontmatter::Note(note) => {
            assert_eq!(note.sync_status, Some(NoteSyncStatus::Live));
        }
        _ => panic!("expected note snapshot"),
    }
    assert!(resolved_from_file.body.contains("File wins."));

    let note_after_file_resolve = service
        .load_project_workspace(&project.id)?
        .notes
        .into_iter()
        .find(|item| item.id == linked.id)
        .expect("linked note should exist");

    let _pending_local_conflict = service.update_note(
        &linked.id,
        NotePatch {
            body: Some("# Architecture\n\nTopside wins.\n".to_string()),
            ..Default::default()
        },
        &note_after_file_resolve.revision,
        Actor::human("tester"),
    )?;

    thread::sleep(Duration::from_millis(50));
    std::fs::write(&doc_path, "# Architecture\n\nExternal stray content.\n")?;

    wait_for("linked note second conflict detection", || {
        let note = service
            .load_project_workspace(&project.id)?
            .notes
            .into_iter()
            .find(|item| item.id == linked.id)
            .expect("linked note should exist");
        Ok(note.sync_status == Some(NoteSyncStatus::Conflict))
    })?;

    let resolved_from_local = service.resolve_note_sync_from_local(&linked.id)?;
    assert!(resolved_from_local.body.contains("Topside wins."));
    let resolved_file = std::fs::read_to_string(&doc_path)?;
    assert!(resolved_file.contains("Topside wins."));
    assert!(!resolved_file.contains("External stray content."));

    let archived = service.archive_entity(
        &linked.id,
        &resolved_from_local.revision,
        Actor::human("tester"),
    )?;
    assert!(archived.archived);
    assert!(std::fs::read_to_string(&doc_path)?.contains("Topside wins."));

    Ok(())
}

#[test]
fn linked_note_watchers_follow_project_source_updates() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root_one = tempfile::TempDir::new()?;
    let repo_root_two = tempfile::TempDir::new()?;
    let first_doc_path = repo_root_one.path().join("docs/ARCHITECTURE.md");
    let second_doc_path = repo_root_two.path().join("docs/ARCHITECTURE.md");

    std::fs::create_dir_all(first_doc_path.parent().expect("first doc parent exists"))?;
    std::fs::create_dir_all(second_doc_path.parent().expect("second doc parent exists"))?;
    std::fs::write(&first_doc_path, "# Architecture\n\nFirst source.\n")?;
    std::fs::write(&second_doc_path, "# Architecture\n\nSecond source.\n")?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Rebound Linked Notes".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root_one.path().to_string_lossy().to_string()),
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let linked = service.link_note_to_repo_file(
        &project.id,
        "docs/ARCHITECTURE.md",
        Actor::human("tester"),
    )?;

    let current_project = project_by_id(&service, &project.id)?;
    let _updated_project = service.update_project(
        &project.id,
        ProjectPatch {
            source_locator: Some(Some(repo_root_two.path().to_string_lossy().to_string())),
            ..Default::default()
        },
        &current_project.revision,
        Actor::human("tester"),
    )?;

    let note = service
        .load_project_workspace(&project.id)?
        .notes
        .into_iter()
        .find(|item| item.id == linked.id)
        .expect("linked note should exist");
    assert!(note.body.contains("Second source."));

    Ok(())
}

#[test]
fn linked_note_dedup_ignores_note_list_pagination() -> Result<()> {
    let (_tmp, service) = common::setup_service_workspace()?;
    let repo_root = tempfile::TempDir::new()?;
    let doc_path = repo_root.path().join("docs/ARCHITECTURE.md");

    std::fs::create_dir_all(doc_path.parent().expect("doc parent exists"))?;
    std::fs::write(&doc_path, "# Architecture\n\nStable linked doc.\n")?;

    let project = service.create_project(
        CreateProjectPayload {
            title: "Dedup Linked Notes".to_string(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(repo_root.path().to_string_lossy().to_string()),
            tags: None,
            body: None,
        },
        Actor::human("tester"),
    )?;

    let linked = service.link_note_to_repo_file(
        &project.id,
        "docs/ARCHITECTURE.md",
        Actor::human("tester"),
    )?;

    for index in 0..5_001 {
        let _ = service.create_note(
            CreateNotePayload {
                title: format!("Filler note {index}"),
                project_id: None,
                tags: None,
                body: None,
            },
            Actor::human("tester"),
        )?;
    }

    let relinked = service.link_note_to_repo_file(
        &project.id,
        "docs/ARCHITECTURE.md",
        Actor::human("tester"),
    )?;
    assert_eq!(relinked.id, linked.id);

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
