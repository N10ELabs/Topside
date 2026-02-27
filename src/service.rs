use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use thiserror::Error;
use ulid::Ulid;

use crate::activity::ActivityDraft;
use crate::config::AppConfig;
use crate::db::{Db, StoredEntityRecord};
use crate::git::read_git_context;
use crate::indexer::{Indexer, WatcherRuntime};
use crate::markdown::{parse_entity_markdown, parse_optional_datetime, render_entity_markdown};
use crate::repo_sync::{derive_sync_source_key, render_synced_task_body, scan_repo_todo_files};
use crate::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, EntityFrontmatter,
    EntitySnapshot, EntityType, NoteDetail, NoteFrontmatter, NoteItem, NotePatch, ParsedEntity,
    ProjectFrontmatter, ProjectPatch, ProjectStatus, ProjectWorkspace, SearchFilters, SearchResult,
    TaskFilters, TaskFrontmatter, TaskItem, TaskPatch, TaskPriority, TaskStatus, TaskSyncKind,
};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("conflict: expected revision {expected}, current revision {current}")]
    Conflict { expected: String, current: String },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ServiceError {
    pub fn conflict_payload(&self) -> Option<(&str, &str)> {
        match self {
            ServiceError::Conflict { expected, current } => Some((expected, current)),
            ServiceError::Other(_) => None,
        }
    }
}

#[derive(Clone)]
pub struct AppService {
    pub config: AppConfig,
    pub db: Db,
    pub indexer: Arc<Indexer>,
}

impl AppService {
    pub fn bootstrap(config: AppConfig) -> Result<Self> {
        config.ensure_workspace_dirs()?;

        let db = Db::open(&config.db_path())?;
        db.run_migrations()?;

        let indexer = Arc::new(Indexer::new(config.clone(), db.clone()));
        if config.index.startup_full_scan {
            indexer.full_scan()?;
        }

        Ok(Self {
            config,
            db,
            indexer,
        })
    }

    pub fn start_watcher(&self) -> Result<WatcherRuntime> {
        self.indexer.clone().start_watcher()
    }

    pub fn reindex_all(&self) -> Result<()> {
        self.indexer.full_scan()
    }

    pub fn import_tree(&self, path: &Path) -> Result<usize> {
        self.indexer.import_tree(path)
    }

    pub fn search_context(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        let limit = limit.unwrap_or(self.config.search.default_limit);
        self.db.search_context(query, filters, limit)
    }

    pub fn read_entity(&self, id_or_path: &str) -> Result<Option<EntitySnapshot>> {
        self.db.read_entity_snapshot(id_or_path)
    }

    pub fn list_tasks(&self, filters: &TaskFilters) -> Result<Vec<TaskItem>> {
        self.db
            .list_tasks(filters, self.config.search.default_limit)
    }

    pub fn list_notes(&self, limit: usize, include_archived: bool) -> Result<Vec<NoteItem>> {
        self.db.list_notes(limit, include_archived)
    }

    pub fn list_projects(
        &self,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<crate::types::ProjectItem>> {
        self.db.list_projects(limit, include_archived)
    }

    pub fn list_recent_activity(
        &self,
        since: Option<chrono::DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<crate::types::ActivityItem>> {
        self.db.list_recent_activity(since, limit)
    }

    pub fn load_project_workspace(&self, project_id: &str) -> Result<ProjectWorkspace> {
        let project = self
            .list_projects(200, false)?
            .into_iter()
            .find(|item| item.id == project_id)
            .with_context(|| format!("project {project_id} not found"))?;

        let mut tasks = self.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project_id.to_string()),
            assignee: None,
            include_archived: false,
            limit: Some(500),
        })?;

        let mut active_tasks = Vec::new();
        let mut done_tasks = Vec::new();
        for task in tasks.drain(..) {
            if task.status == TaskStatus::Done {
                done_tasks.push(task);
            } else {
                active_tasks.push(task);
            }
        }

        active_tasks.sort_by(|left, right| {
            effective_task_sort_order(left)
                .cmp(&effective_task_sort_order(right))
                .then(left.created_at.cmp(&right.created_at))
        });
        done_tasks.sort_by(|left, right| {
            effective_completed_at(right)
                .cmp(&effective_completed_at(left))
                .then(right.updated_at.cmp(&left.updated_at))
        });

        let mut notes = self.list_notes(200, false)?;
        notes.retain(|note| note.project_id.as_deref() == Some(project_id));

        let mut note_details = Vec::new();
        for note in notes {
            let Some(snapshot) = self.read_entity(&note.id)? else {
                continue;
            };
            note_details.push(NoteDetail {
                id: snapshot.id,
                title: snapshot.title,
                project_id: snapshot.frontmatter.project_id().map(ToString::to_string),
                body: snapshot.body,
                path: snapshot.path,
                updated_at: snapshot.frontmatter.updated_at(),
                revision: snapshot.revision,
                archived: snapshot.archived,
            });
        }
        note_details.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));

        let suggested_open_note_id = note_details.first().map(|note| note.id.clone());

        Ok(ProjectWorkspace {
            project,
            active_tasks,
            done_tasks,
            notes: note_details,
            suggested_open_note_id,
        })
    }

    pub fn create_task_after(
        &self,
        project_id: &str,
        title: String,
        after_task_id: Option<&str>,
        actor: Actor,
    ) -> Result<(ProjectWorkspace, String), ServiceError> {
        let ordered_active_ids = self
            .load_project_workspace(project_id)?
            .active_tasks
            .into_iter()
            .map(|task| task.id)
            .collect::<Vec<_>>();

        let created = self.create_task(
            CreateTaskPayload {
                title,
                project_id: project_id.to_string(),
                status: Some(TaskStatus::Todo),
                priority: Some(TaskPriority::P2),
                assignee: Some("agent:unassigned".to_string()),
                due_at: None,
                sort_order: Some((ordered_active_ids.len() + 1) as i64),
                sync_kind: None,
                sync_path: None,
                sync_key: None,
                sync_managed: None,
                tags: None,
                body: None,
            },
            actor,
        )?;

        if let Some(after_task_id) = after_task_id {
            let mut ordered_ids = ordered_active_ids;
            let insert_at = ordered_ids
                .iter()
                .position(|id| id == after_task_id)
                .map(|index| index + 1)
                .unwrap_or(ordered_ids.len());
            ordered_ids.insert(insert_at, created.id.clone());
            self.reorder_project_tasks_internal(project_id, &ordered_ids, None)?;
        }

        Ok((
            self.load_project_workspace(project_id)
                .map_err(ServiceError::Other)?,
            created.id,
        ))
    }

    pub fn reorder_project_tasks(
        &self,
        project_id: &str,
        ordered_active_task_ids: &[String],
        actor: Actor,
    ) -> Result<ProjectWorkspace, ServiceError> {
        self.reorder_project_tasks_internal(project_id, ordered_active_task_ids, Some(actor))?;
        self.load_project_workspace(project_id)
            .map_err(ServiceError::Other)
    }

    pub fn create_project(
        &self,
        payload: CreateProjectPayload,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let now = Utc::now();
        let id = format!("{}_{}", EntityType::Project.prefix(), Ulid::new());
        let title_slug = slugify(&payload.title);
        let path = self
            .config
            .projects_dir()
            .join(format!("{id}-{title_slug}.md"));
        self.config.ensure_within_workspace(&path)?;

        let sync_source_key = match (&payload.source_kind, &payload.source_locator) {
            (Some(kind), Some(locator)) => Some(derive_sync_source_key(kind.as_str(), locator)),
            _ => None,
        };
        let mut fm = EntityFrontmatter::Project(ProjectFrontmatter {
            id: id.clone(),
            entity_type: EntityType::Project,
            title: payload.title,
            status: ProjectStatus::Active,
            owner: payload.owner,
            source_kind: payload.source_kind,
            source_locator: payload.source_locator,
            sync_source_key,
            last_synced_at: None,
            last_sync_summary: None,
            tags: payload.tags,
            created_at: now,
            updated_at: now,
            revision: String::new(),
        });

        let body = payload.body.unwrap_or_default();
        let content = render_entity_markdown(&mut fm, &body)?;
        atomic_write(&path, &content)?;
        let indexed = self.indexer.index_file(&path)?;

        self.record_entity_activity(
            actor,
            EntityActivityMeta {
                action: "create_project",
                entity_type: indexed.entity_type,
                entity_id: &indexed.id,
                path: &path,
                before_revision: None,
                after_revision: Some(indexed.revision.clone()),
                summary: "Created project",
            },
        )?;

        self.db
            .read_entity_snapshot(&id)?
            .context("created project not found after indexing")
            .map_err(ServiceError::Other)
    }

    pub fn create_task(
        &self,
        payload: CreateTaskPayload,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        self.ensure_project_exists(&payload.project_id)?;
        let next_sort_order = match payload.sort_order {
            Some(sort_order) => sort_order,
            None => self.next_task_sort_order(&payload.project_id)?,
        };

        let now = Utc::now();
        let id = format!("{}_{}", EntityType::Task.prefix(), Ulid::new());
        let title_slug = slugify(&payload.title);
        let path = self
            .config
            .tasks_dir()
            .join(&payload.project_id)
            .join(format!("{id}-{title_slug}.md"));
        self.config.ensure_within_workspace(&path)?;

        let status = payload.status.unwrap_or_default();
        let completed_at = if status == TaskStatus::Done {
            Some(now)
        } else {
            None
        };

        let mut fm = EntityFrontmatter::Task(TaskFrontmatter {
            id: id.clone(),
            entity_type: EntityType::Task,
            title: payload.title,
            project_id: payload.project_id,
            status,
            priority: payload.priority.unwrap_or_default(),
            assignee: payload
                .assignee
                .unwrap_or_else(|| "agent:unassigned".to_string()),
            due_at: payload.due_at,
            sort_order: next_sort_order,
            completed_at,
            sync_kind: payload.sync_kind,
            sync_path: payload.sync_path,
            sync_key: payload.sync_key,
            sync_managed: payload.sync_managed.unwrap_or(false),
            tags: payload.tags,
            created_at: now,
            updated_at: now,
            revision: String::new(),
        });

        let body = payload.body.unwrap_or_default();
        let content = render_entity_markdown(&mut fm, &body)?;
        atomic_write(&path, &content)?;
        let indexed = self.indexer.index_file(&path)?;

        self.record_entity_activity(
            actor,
            EntityActivityMeta {
                action: "create_task",
                entity_type: indexed.entity_type,
                entity_id: &indexed.id,
                path: &path,
                before_revision: None,
                after_revision: Some(indexed.revision.clone()),
                summary: "Created task",
            },
        )?;

        self.db
            .read_entity_snapshot(&id)?
            .context("created task not found after indexing")
            .map_err(ServiceError::Other)
    }

    pub fn create_note(
        &self,
        payload: CreateNotePayload,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        if let Some(project_id) = &payload.project_id {
            self.ensure_project_exists(project_id)?;
        }

        let now = Utc::now();
        let id = format!("{}_{}", EntityType::Note.prefix(), Ulid::new());
        let title_slug = slugify(&payload.title);

        let base_dir = match &payload.project_id {
            Some(project_id) => self.config.notes_dir().join(project_id),
            None => self.config.notes_dir().join("inbox"),
        };

        let path = base_dir.join(format!("{id}-{title_slug}.md"));
        self.config.ensure_within_workspace(&path)?;

        let mut fm = EntityFrontmatter::Note(NoteFrontmatter {
            id: id.clone(),
            entity_type: EntityType::Note,
            title: payload.title,
            project_id: payload.project_id,
            tags: payload.tags,
            created_at: now,
            updated_at: now,
            revision: String::new(),
        });

        let body = payload.body.unwrap_or_default();
        let content = render_entity_markdown(&mut fm, &body)?;
        atomic_write(&path, &content)?;
        let indexed = self.indexer.index_file(&path)?;

        self.record_entity_activity(
            actor,
            EntityActivityMeta {
                action: "create_note",
                entity_type: indexed.entity_type,
                entity_id: &indexed.id,
                path: &path,
                before_revision: None,
                after_revision: Some(indexed.revision.clone()),
                summary: "Created note",
            },
        )?;

        self.db
            .read_entity_snapshot(&id)?
            .context("created note not found after indexing")
            .map_err(ServiceError::Other)
    }

    pub fn update_task(
        &self,
        id: &str,
        patch: TaskPatch,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let (record, parsed) = self
            .db
            .parse_entity_from_disk(id)?
            .context("task not found")?;

        if record.entity_type != EntityType::Task {
            return Err(anyhow::anyhow!("entity {id} is not a task").into());
        }

        self.enforce_revision(expected_revision, &parsed)?;

        let (mut frontmatter, mut body) = split_task(parsed.frontmatter, parsed.body)?;

        if let Some(value) = patch.title {
            frontmatter.title = value;
        }
        if let Some(value) = patch.status {
            if value == TaskStatus::Done && frontmatter.status != TaskStatus::Done {
                frontmatter.completed_at = Some(Utc::now());
            } else if value != TaskStatus::Done && frontmatter.status == TaskStatus::Done {
                frontmatter.completed_at = None;
            }
            frontmatter.status = value;
        }
        if let Some(value) = patch.priority {
            frontmatter.priority = value;
        }
        if let Some(value) = patch.assignee {
            frontmatter.assignee = value;
        }
        if let Some(value) = patch.due_at {
            frontmatter.due_at = parse_optional_datetime(&value)?;
        }
        if let Some(value) = patch.sort_order {
            frontmatter.sort_order = value;
        }
        if let Some(value) = patch.tags {
            frontmatter.tags = Some(value);
        }
        if let Some(value) = patch.body {
            body = value;
        }

        frontmatter.updated_at = Utc::now();

        let before = record.revision.clone();
        let mut entity = EntityFrontmatter::Task(frontmatter);
        let rendered = render_entity_markdown(&mut entity, &body)?;
        atomic_write(&record.path, &rendered)?;
        let indexed = self.indexer.index_file(&record.path)?;

        self.record_entity_activity(
            actor,
            EntityActivityMeta {
                action: "update_task",
                entity_type: EntityType::Task,
                entity_id: id,
                path: &record.path,
                before_revision: Some(before),
                after_revision: Some(indexed.revision.clone()),
                summary: "Updated task",
            },
        )?;

        self.db
            .read_entity_snapshot(id)?
            .context("updated task not found after indexing")
            .map_err(ServiceError::Other)
    }

    pub fn update_note(
        &self,
        id: &str,
        patch: NotePatch,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let (record, parsed) = self
            .db
            .parse_entity_from_disk(id)?
            .context("note not found")?;

        if record.entity_type != EntityType::Note {
            return Err(anyhow::anyhow!("entity {id} is not a note").into());
        }

        self.enforce_revision(expected_revision, &parsed)?;

        let (mut frontmatter, mut body) = split_note(parsed.frontmatter, parsed.body)?;

        if let Some(value) = patch.title {
            frontmatter.title = value;
        }
        if let Some(value) = patch.project_id {
            if !value.is_empty() {
                self.ensure_project_exists(&value)?;
                frontmatter.project_id = Some(value);
            }
        }
        if let Some(value) = patch.tags {
            frontmatter.tags = Some(value);
        }
        if let Some(value) = patch.body {
            body = value;
        }

        frontmatter.updated_at = Utc::now();

        let before = record.revision.clone();
        let mut entity = EntityFrontmatter::Note(frontmatter);
        let rendered = render_entity_markdown(&mut entity, &body)?;
        atomic_write(&record.path, &rendered)?;
        let indexed = self.indexer.index_file(&record.path)?;

        self.record_entity_activity(
            actor,
            EntityActivityMeta {
                action: "update_note",
                entity_type: EntityType::Note,
                entity_id: id,
                path: &record.path,
                before_revision: Some(before),
                after_revision: Some(indexed.revision.clone()),
                summary: "Updated note",
            },
        )?;

        self.db
            .read_entity_snapshot(id)?
            .context("updated note not found after indexing")
            .map_err(ServiceError::Other)
    }

    pub fn update_project(
        &self,
        id: &str,
        patch: ProjectPatch,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let (record, parsed) = self
            .db
            .parse_entity_from_disk(id)?
            .context("project not found")?;

        if record.entity_type != EntityType::Project {
            return Err(anyhow::anyhow!("entity {id} is not a project").into());
        }

        self.enforce_revision(expected_revision, &parsed)?;

        let (mut frontmatter, mut body) = split_project(parsed.frontmatter, parsed.body)?;

        if let Some(value) = patch.title {
            frontmatter.title = value;
        }
        if let Some(value) = patch.status {
            frontmatter.status = value;
        }
        if let Some(value) = patch.owner {
            frontmatter.owner = value;
        }
        if let Some(value) = patch.source_kind {
            frontmatter.source_kind = value;
        }
        if let Some(value) = patch.source_locator {
            frontmatter.source_locator = value;
        }
        if let Some(value) = patch.sync_source_key {
            frontmatter.sync_source_key = value;
        }
        if let Some(value) = patch.last_synced_at {
            frontmatter.last_synced_at = value;
        }
        if let Some(value) = patch.last_sync_summary {
            frontmatter.last_sync_summary = value;
        }
        if let Some(value) = patch.tags {
            frontmatter.tags = Some(value);
        }
        if let Some(value) = patch.body {
            body = value;
        }

        frontmatter.updated_at = Utc::now();

        let before = record.revision.clone();
        let mut entity = EntityFrontmatter::Project(frontmatter);
        let rendered = render_entity_markdown(&mut entity, &body)?;
        atomic_write(&record.path, &rendered)?;
        let indexed = self.indexer.index_file(&record.path)?;

        self.record_entity_activity(
            actor,
            EntityActivityMeta {
                action: "update_project",
                entity_type: EntityType::Project,
                entity_id: id,
                path: &record.path,
                before_revision: Some(before),
                after_revision: Some(indexed.revision.clone()),
                summary: "Updated project",
            },
        )?;

        self.db
            .read_entity_snapshot(id)?
            .context("updated project not found after indexing")
            .map_err(ServiceError::Other)
    }

    pub fn sync_project_from_source(
        &self,
        project_id: &str,
        actor: Actor,
    ) -> Result<ProjectSyncReport, ServiceError> {
        let project = self
            .list_projects(200, false)?
            .into_iter()
            .find(|item| item.id == project_id)
            .with_context(|| format!("project {project_id} not found"))?;

        let source_kind = project
            .source_kind
            .clone()
            .with_context(|| "project has no linked source")?;
        if source_kind != crate::types::ProjectSourceKind::Local {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "phase 1 sync is only available for linked local folders"
            )));
        }

        let source_locator = project
            .source_locator
            .clone()
            .with_context(|| "project has no linked source path")?;
        let source_root = PathBuf::from(&source_locator);
        if !source_root.is_dir() {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "linked source path is not a directory"
            )));
        }

        let scan = scan_repo_todo_files(&source_root)?;
        let existing_tasks = self.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project_id.to_string()),
            assignee: None,
            include_archived: false,
            limit: Some(5_000),
        })?;

        let mut existing_by_sync = std::collections::HashMap::new();
        for task in existing_tasks {
            if task.sync_managed
                && task.sync_kind == Some(TaskSyncKind::RepoMarkdown)
                && task.sync_path.is_some()
                && task.sync_key.is_some()
            {
                existing_by_sync.insert(
                    (
                        task.sync_path.clone().unwrap_or_default(),
                        task.sync_key.clone().unwrap_or_default(),
                    ),
                    task,
                );
            }
        }

        let mut created = 0usize;
        let mut updated = 0usize;

        for candidate in &scan.task_candidates {
            let desired_status = if candidate.completed {
                TaskStatus::Done
            } else {
                TaskStatus::Todo
            };
            let match_key = (candidate.relative_path.clone(), candidate.sync_key.clone());

            if let Some(existing) = existing_by_sync.get(&match_key) {
                let mut patch = TaskPatch::default();
                let mut changed = false;

                if existing.title != candidate.title {
                    patch.title = Some(candidate.title.clone());
                    changed = true;
                }
                if existing.status != desired_status {
                    patch.status = Some(desired_status);
                    changed = true;
                }

                if changed {
                    self.update_task(&existing.id, patch, &existing.revision, actor.clone())?;
                    updated += 1;
                }
                continue;
            }

            self.create_task(
                CreateTaskPayload {
                    title: candidate.title.clone(),
                    project_id: project_id.to_string(),
                    status: Some(desired_status),
                    priority: Some(TaskPriority::P2),
                    assignee: Some("agent:unassigned".to_string()),
                    due_at: None,
                    sort_order: None,
                    sync_kind: Some(TaskSyncKind::RepoMarkdown),
                    sync_path: Some(candidate.relative_path.clone()),
                    sync_key: Some(candidate.sync_key.clone()),
                    sync_managed: Some(true),
                    tags: None,
                    body: Some(render_synced_task_body(
                        &candidate.relative_path,
                        &candidate.section_path,
                    )),
                },
                actor.clone(),
            )?;
            created += 1;
        }

        let synced_at = Utc::now();
        let summary = format!(
            "Scanned {} file(s), found {} repo task(s): {} created, {} updated.",
            scan.files_scanned,
            scan.task_candidates.len(),
            created,
            updated
        );
        let source_key = derive_sync_source_key(source_kind.as_str(), &source_locator);

        let project_snapshot = self
            .read_entity(project_id)?
            .context("project not found during sync")?;
        self.update_project(
            project_id,
            ProjectPatch {
                sync_source_key: Some(Some(source_key)),
                last_synced_at: Some(Some(synced_at)),
                last_sync_summary: Some(Some(summary.clone())),
                ..ProjectPatch::default()
            },
            &project_snapshot.revision,
            actor,
        )?;

        Ok(ProjectSyncReport {
            project_id: project_id.to_string(),
            files_scanned: scan.files_scanned,
            tasks_found: scan.task_candidates.len(),
            created,
            updated,
            synced_at,
            summary,
        })
    }

    pub fn archive_entity(
        &self,
        id: &str,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let record = self.db.get_entity_record(id)?.context("entity not found")?;

        let raw = std::fs::read_to_string(&record.path)
            .with_context(|| format!("failed reading {}", record.path.display()))?;
        let parsed = parse_entity_markdown(&raw)?;
        self.enforce_revision(expected_revision, &parsed)?;

        let archive_dir = self.config.archive_dir().join(record.entity_type.as_str());
        std::fs::create_dir_all(&archive_dir)
            .with_context(|| format!("failed creating {}", archive_dir.display()))?;

        let file_name = record
            .path
            .file_name()
            .and_then(|v| v.to_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("{}-{}.md", record.id, Ulid::new()));

        let mut target = archive_dir.join(&file_name);
        if target.exists() {
            target = archive_dir.join(format!("{}-{}", Ulid::new(), file_name));
        }

        self.config.ensure_within_workspace(&target)?;
        std::fs::rename(&record.path, &target).with_context(|| {
            format!(
                "failed moving {} to {}",
                record.path.display(),
                target.display()
            )
        })?;

        self.indexer.remove_path(&record.path)?;
        let indexed = self.indexer.index_file(&target)?;

        self.record_entity_activity(
            actor,
            EntityActivityMeta {
                action: "archive_entity",
                entity_type: record.entity_type,
                entity_id: &record.id,
                path: &target,
                before_revision: Some(record.revision),
                after_revision: Some(indexed.revision),
                summary: "Archived entity",
            },
        )?;

        self.db
            .read_entity_snapshot(id)?
            .context("archived entity not found after indexing")
            .map_err(ServiceError::Other)
    }

    fn next_task_sort_order(&self, project_id: &str) -> Result<i64, ServiceError> {
        let workspace = self.load_project_workspace(project_id)?;
        let next = workspace
            .active_tasks
            .iter()
            .map(effective_task_sort_order)
            .max()
            .unwrap_or(0)
            + 1;
        Ok(next)
    }

    fn reorder_project_tasks_internal(
        &self,
        project_id: &str,
        ordered_active_task_ids: &[String],
        actor: Option<Actor>,
    ) -> Result<(), ServiceError> {
        self.ensure_project_exists(project_id)?;
        let workspace = self.load_project_workspace(project_id)?;
        let expected = workspace
            .active_tasks
            .iter()
            .map(|task| task.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let provided = ordered_active_task_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();

        if expected != provided {
            return Err(anyhow::anyhow!("reorder payload did not match active task set").into());
        }

        let now = Utc::now();
        for (index, task_id) in ordered_active_task_ids.iter().enumerate() {
            let (record, parsed) = self
                .db
                .parse_entity_from_disk(task_id)?
                .context("task not found during reorder")?;
            let (mut frontmatter, body) = split_task(parsed.frontmatter, parsed.body)?;
            frontmatter.sort_order = (index as i64) + 1;
            frontmatter.updated_at = now;
            let mut entity = EntityFrontmatter::Task(frontmatter);
            let rendered = render_entity_markdown(&mut entity, &body)?;
            atomic_write(&record.path, &rendered)?;
            self.indexer.index_file(&record.path)?;
        }

        if let Some(actor) = actor {
            let project = workspace.project;
            let project_path = PathBuf::from(project.path);
            self.record_entity_activity(
                actor,
                EntityActivityMeta {
                    action: "reorder_tasks",
                    entity_type: EntityType::Project,
                    entity_id: &project.id,
                    path: &project_path,
                    before_revision: None,
                    after_revision: None,
                    summary: "Reordered project tasks",
                },
            )?;
        }

        Ok(())
    }

    fn ensure_project_exists(&self, project_id: &str) -> Result<()> {
        let record = self
            .db
            .get_entity_record(project_id)?
            .with_context(|| format!("project {project_id} not found"))?;
        if record.entity_type != EntityType::Project {
            anyhow::bail!("{project_id} exists but is not a project");
        }
        Ok(())
    }

    fn enforce_revision(&self, expected: &str, parsed: &ParsedEntity) -> Result<(), ServiceError> {
        if parsed.revision == expected {
            return Ok(());
        }

        Err(ServiceError::Conflict {
            expected: expected.to_string(),
            current: parsed.revision.clone(),
        })
    }

    fn record_entity_activity(&self, actor: Actor, meta: EntityActivityMeta<'_>) -> Result<()> {
        let git = read_git_context(&self.config.workspace_root);
        let draft = ActivityDraft::new(actor, meta.action, meta.summary)
            .with_entity(meta.entity_type, meta.entity_id.to_string())
            .with_path(meta.path.to_string_lossy().to_string())
            .with_revisions(meta.before_revision, meta.after_revision)
            .with_git(git.branch, git.commit);
        self.db.record_activity(draft)?;
        Ok(())
    }
}

struct EntityActivityMeta<'a> {
    action: &'a str,
    entity_type: EntityType,
    entity_id: &'a str,
    path: &'a Path,
    before_revision: Option<String>,
    after_revision: Option<String>,
    summary: &'a str,
}

#[derive(Debug, Clone)]
pub struct ProjectSyncReport {
    pub project_id: String,
    pub files_scanned: usize,
    pub tasks_found: usize,
    pub created: usize,
    pub updated: usize,
    pub synced_at: chrono::DateTime<Utc>,
    pub summary: String,
}

fn split_task(frontmatter: EntityFrontmatter, body: String) -> Result<(TaskFrontmatter, String)> {
    match frontmatter {
        EntityFrontmatter::Task(task) => Ok((task, body)),
        _ => anyhow::bail!("expected task frontmatter"),
    }
}

fn split_note(frontmatter: EntityFrontmatter, body: String) -> Result<(NoteFrontmatter, String)> {
    match frontmatter {
        EntityFrontmatter::Note(note) => Ok((note, body)),
        _ => anyhow::bail!("expected note frontmatter"),
    }
}

fn split_project(
    frontmatter: EntityFrontmatter,
    body: String,
) -> Result<(ProjectFrontmatter, String)> {
    match frontmatter {
        EntityFrontmatter::Project(project) => Ok((project, body)),
        _ => anyhow::bail!("expected project frontmatter"),
    }
}

fn effective_task_sort_order(task: &TaskItem) -> i64 {
    if task.sort_order > 0 {
        task.sort_order
    } else {
        task.created_at.timestamp_millis()
    }
}

fn effective_completed_at(task: &TaskItem) -> chrono::DateTime<Utc> {
    task.completed_at.unwrap_or(task.updated_at)
}

fn slugify(value: &str) -> String {
    let mut slug = value
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();

    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }

    let slug = slug.trim_matches('-').chars().take(64).collect::<String>();
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug
    }
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating parent dir {}", parent.display()))?;
    }

    let mut tmp = PathBuf::from(path);
    let extension = format!("tmp-{}", Ulid::new());
    tmp.set_extension(extension);

    std::fs::write(&tmp, content).with_context(|| format!("failed writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed renaming {} -> {}", tmp.display(), path.display()))?;

    Ok(())
}

#[allow(dead_code)]
fn _task_defaults(_status: TaskStatus, _priority: TaskPriority) {}

#[allow(dead_code)]
fn _record_passthrough(record: StoredEntityRecord) -> StoredEntityRecord {
    record
}
