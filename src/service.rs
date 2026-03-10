use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tracing::warn;
use ulid::Ulid;

use crate::activity::ActivityDraft;
use crate::codex::{
    CodexSessionCounts, CodexSessionPatch, CodexSessionRecord, CodexSessionStore,
    reconcile_project_codex_history,
};
use crate::config::AppConfig;
use crate::constants::UNBOUNDED_QUERY_LIMIT;
use crate::db::{Db, StoredEntityRecord};
use crate::git::{GitContext, read_git_context};
use crate::indexer::{Indexer, WatcherRuntime};
use crate::markdown::{parse_entity_markdown, parse_optional_datetime, render_entity_markdown};
use crate::repo_sync::{
    RepoMarkdownDocCandidate, derive_sync_source_key, list_repo_markdown_docs,
    render_synced_task_body, scan_repo_todo_files,
};
use crate::task_sync::{
    ManagedTodoEntryKind, ManagedTodoRenderEntry, OUTBOUND_DEBOUNCE_MS, ParsedManagedTodoEntry,
    WATCHER_DEBOUNCE_MS, compute_file_hash, compute_file_hash_from_path, ensure_parent_dir,
    ensure_sync_key_for_title, is_heading_title, legacy_managed_todo_sidecar_path,
    managed_todo_sidecar_path, normalize_managed_task_sync_file, parse_managed_todo,
    parse_managed_todo_sidecar, render_entry_from_task, render_managed_todo,
    render_managed_todo_sidecar, resolve_managed_file_path, task_title_from_entry,
};
use crate::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, EntityFrontmatter,
    EntitySnapshot, EntityType, NoteFrontmatter, NoteItem, NotePatch, NoteSyncKind, NoteSyncStatus,
    ParsedEntity, ProjectFrontmatter, ProjectPatch, ProjectStatus, ProjectWorkspace, SearchFilters,
    SearchResult, TaskFilters, TaskFrontmatter, TaskItem, TaskPatch, TaskPriority, TaskStatus,
    TaskSyncKind, TaskSyncMode, TaskSyncStatus,
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

#[derive(Debug, Clone)]
pub struct TaskUpdateRequest {
    pub id: String,
    pub expected_revision: String,
    pub patch: TaskPatch,
}

#[derive(Debug, Clone)]
pub struct ArchiveEntityRequest {
    pub id: String,
    pub expected_revision: String,
}

#[derive(Debug, Clone)]
pub struct RestoreEntityRequest {
    pub id: String,
    pub expected_revision: String,
}

type ManagedTaskSyncDefaults = (
    Option<TaskSyncKind>,
    Option<String>,
    Option<String>,
    Option<bool>,
);

struct NoteWriteContext {
    before_revision: Option<String>,
    archived: bool,
    actor: Actor,
    action: &'static str,
    summary: &'static str,
}

#[derive(Clone)]
pub struct AppService {
    pub config: AppConfig,
    pub db: Db,
    pub indexer: Arc<Indexer>,
    git_context_cache: Arc<Mutex<Option<GitContextCacheEntry>>>,
    task_sync_runtime: Arc<Mutex<ManagedTaskSyncRuntime>>,
    note_sync_runtime: Arc<Mutex<ManagedNoteSyncRuntime>>,
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
            git_context_cache: Arc::new(Mutex::new(None)),
            task_sync_runtime: Arc::new(Mutex::new(ManagedTaskSyncRuntime::default())),
            note_sync_runtime: Arc::new(Mutex::new(ManagedNoteSyncRuntime::default())),
        })
    }

    pub fn start_watcher(&self) -> Result<WatcherRuntime> {
        let runtime = self.indexer.clone().start_watcher()?;
        self.reconcile_managed_task_sync_project_defaults()?;
        self.restore_managed_task_sync_watchers()?;
        self.restore_note_sync_watchers()?;
        Ok(runtime)
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
        let project = self.get_project_item(project_id)?;

        let mut tasks = self.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project_id.to_string()),
            assignee: None,
            include_archived: false,
            limit: Some(UNBOUNDED_QUERY_LIMIT),
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

        let note_details =
            self.db
                .list_note_details_for_project(project_id, UNBOUNDED_QUERY_LIMIT, false)?;

        let suggested_open_note_id = note_details.first().map(|note| note.id.clone());
        let codex_sessions = self
            .list_codex_sessions(project_id)
            .map_err(ServiceError::Other)?;

        Ok(ProjectWorkspace {
            project,
            active_tasks,
            done_tasks,
            notes: note_details,
            codex_sessions,
            suggested_open_note_id,
        })
    }

    pub fn list_codex_sessions(&self, project_id: &str) -> Result<Vec<CodexSessionRecord>> {
        let store = self.codex_session_store();
        if let Ok(project_root) = self.local_project_source_root(project_id) {
            if let Err(error) = reconcile_project_codex_history(&store, project_id, &project_root) {
                warn!(
                    error = %error,
                    project_id,
                    "failed reconciling codex session history for project"
                );
            }
        }
        store.list_project_sessions(project_id)
    }

    pub fn list_all_codex_sessions(&self) -> Result<Vec<CodexSessionRecord>> {
        self.codex_session_store().list_all_sessions()
    }

    pub fn list_codex_session_counts(&self) -> Result<HashMap<String, CodexSessionCounts>> {
        self.codex_session_store().list_counts_by_project()
    }

    pub fn get_codex_session(&self, session_id: &str) -> Result<Option<CodexSessionRecord>> {
        self.codex_session_store().get_session(session_id)
    }

    pub fn update_codex_session(
        &self,
        session_id: &str,
        patch: CodexSessionPatch,
    ) -> Result<CodexSessionRecord, ServiceError> {
        self.codex_session_store()
            .update_session(session_id, patch)
            .map_err(ServiceError::Other)
    }

    pub fn local_project_source_root(&self, project_id: &str) -> Result<PathBuf, ServiceError> {
        let project = self.get_project_item(project_id)?;
        if project.source_kind != Some(crate::types::ProjectSourceKind::Local) {
            return Err(anyhow::anyhow!(
                "Codex sessions require a project linked to a local folder"
            )
            .into());
        }
        let source_locator = project
            .source_locator
            .with_context(|| format!("project {project_id} is missing source_locator"))
            .map_err(ServiceError::Other)?;
        let path = PathBuf::from(&source_locator)
            .canonicalize()
            .with_context(|| format!("failed canonicalizing {}", source_locator))
            .map_err(ServiceError::Other)?;
        Ok(path)
    }

    pub fn suggest_codex_session_title(
        &self,
        project_id: &str,
        task_id: Option<&str>,
    ) -> Result<String, ServiceError> {
        if let Some(task_id) = task_id {
            if let Some(task) = self
                .list_tasks(&TaskFilters {
                    status: None,
                    priority: None,
                    project_id: Some(project_id.to_string()),
                    assignee: None,
                    include_archived: false,
                    limit: Some(UNBOUNDED_QUERY_LIMIT),
                })?
                .into_iter()
                .find(|task| task.id == task_id)
            {
                return Ok(task.title);
            }
        }
        Ok("New Codex session".to_string())
    }

    pub fn build_codex_execute_prompt(
        &self,
        _project_id: &str,
        _task_id: &str,
        task_title: &str,
    ) -> Result<String, ServiceError> {
        Ok(format!("Execute the following task: {task_title}"))
    }

    pub fn create_task_after(
        &self,
        project_id: &str,
        title: String,
        after_task_id: Option<&str>,
        actor: Actor,
    ) -> Result<(ProjectWorkspace, String), ServiceError> {
        let (sync_kind, sync_path, sync_key, sync_managed) = self
            .managed_task_sync_defaults_for_new_task(project_id, &title)
            .map_err(ServiceError::Other)?;
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
                priority: Some(TaskPriority::P0),
                assignee: Some("agent:unassigned".to_string()),
                due_at: None,
                sort_order: Some(self.next_task_sort_order(project_id)?),
                sync_kind,
                sync_path,
                sync_key,
                sync_managed,
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

        self.queue_managed_task_sync_outbound(project_id, "create_task");

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
        self.queue_managed_task_sync_outbound(project_id, "reorder_tasks");
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
            icon: payload.icon,
            source_kind: payload.source_kind,
            source_locator: payload.source_locator,
            sync_source_key,
            last_synced_at: None,
            last_sync_summary: None,
            task_sync_mode: None,
            task_sync_file: None,
            task_sync_enabled: false,
            task_sync_status: None,
            task_sync_last_seen_hash: None,
            task_sync_last_inbound_at: None,
            task_sync_last_outbound_at: None,
            task_sync_conflict_summary: None,
            task_sync_conflict_at: None,
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

    pub fn ensure_local_project_user_files(
        &self,
        project_id: &str,
        actor: Actor,
    ) -> Result<(), ServiceError> {
        let mut project = self.get_project_item(project_id)?;
        if project.source_kind != Some(crate::types::ProjectSourceKind::Local) {
            return Ok(());
        }

        let needs_managed_task_sync = !project.task_sync_enabled
            || project.task_sync_mode != Some(TaskSyncMode::ManagedTodoFile);
        if needs_managed_task_sync {
            let _ = self.enable_managed_task_sync(project_id, &project.revision)?;
            project = self.get_project_item(project_id)?;
        }

        let sync_path = self.managed_task_sync_file_path(&project)?;
        if !sync_path.exists() {
            self.write_managed_task_sync_file_from_local(&project, false)?;
        }

        let workspace = self
            .load_project_workspace(project_id)
            .map_err(ServiceError::Other)?;
        if workspace.notes.is_empty() {
            let mut linked = false;
            if let Some(candidate) = self
                .list_linkable_note_files(project_id)?
                .into_iter()
                .next()
            {
                self.link_note_to_repo_file(project_id, &candidate.relative_path, actor.clone())?;
                linked = true;
            }

            if !linked {
                self.create_note(
                    CreateNotePayload {
                        title: "Project notes".to_string(),
                        project_id: Some(project_id.to_string()),
                        tags: None,
                        body: Some(String::new()),
                    },
                    actor,
                )?;
            }
        }

        Ok(())
    }

    pub fn create_task(
        &self,
        payload: CreateTaskPayload,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        self.create_tasks(vec![payload], actor)?
            .into_iter()
            .next()
            .context("created task missing from batch result")
            .map_err(ServiceError::Other)
    }

    pub fn create_tasks(
        &self,
        payloads: Vec<CreateTaskPayload>,
        actor: Actor,
    ) -> Result<Vec<EntitySnapshot>, ServiceError> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }

        let mut next_sort_orders = std::collections::HashMap::<String, i64>::new();
        let mut pending = Vec::with_capacity(payloads.len());
        let mut paths = Vec::with_capacity(payloads.len());

        for payload in payloads {
            let next_sort_order = match next_sort_orders.entry(payload.project_id.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    self.ensure_project_exists(&payload.project_id)?;
                    entry.insert(self.next_task_sort_order(&payload.project_id)?)
                }
            };

            let assigned_sort_order = match payload.sort_order {
                Some(sort_order) => {
                    if sort_order >= *next_sort_order {
                        *next_sort_order = sort_order + 1;
                    }
                    sort_order
                }
                None => {
                    let sort_order = *next_sort_order;
                    *next_sort_order += 1;
                    sort_order
                }
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

            let mut frontmatter = EntityFrontmatter::Task(TaskFrontmatter {
                id,
                entity_type: EntityType::Task,
                title: payload.title,
                project_id: payload.project_id,
                status,
                priority: payload.priority.unwrap_or_default(),
                assignee: payload
                    .assignee
                    .unwrap_or_else(|| "agent:unassigned".to_string()),
                due_at: payload.due_at,
                sort_order: assigned_sort_order,
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
            let content = render_entity_markdown(&mut frontmatter, &body)?;
            atomic_write(&path, &content)?;

            paths.push(path.clone());
            pending.push(PendingMutation {
                path,
                body,
                frontmatter,
                before_revision: None,
                archived: false,
            });
        }

        let indexed = self.indexer.index_files(&paths)?;
        let mut created = Vec::with_capacity(indexed.len());
        let mut activity = Vec::with_capacity(indexed.len());
        for (pending, indexed) in pending.into_iter().zip(indexed) {
            let revision = indexed.revision.clone();
            activity.push(OwnedEntityActivityMeta {
                action: "create_task",
                entity_type: EntityType::Task,
                entity_id: pending.frontmatter.id().to_string(),
                path: pending.path.clone(),
                before_revision: None,
                after_revision: Some(revision.clone()),
                summary: "Created task",
            });
            created.push(snapshot_from_parts(
                &pending.path,
                pending.body,
                pending.frontmatter,
                revision,
                pending.archived,
            ));
        }

        self.record_entity_activities(&actor, activity)?;
        Ok(created)
    }

    pub fn create_note(
        &self,
        payload: CreateNotePayload,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        if let Some(project_id) = payload.project_id.as_deref() {
            self.ensure_project_exists(project_id)?;
            let project = self.get_project_item(project_id)?;
            if project.source_kind == Some(crate::types::ProjectSourceKind::Local) {
                return self.create_note_linked_to_project_docs(project, payload, actor);
            }
        }

        self.create_standalone_note(payload, actor)
    }

    fn create_standalone_note(
        &self,
        payload: CreateNotePayload,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
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
            sync_kind: None,
            sync_path: None,
            sync_status: None,
            sync_last_seen_hash: None,
            sync_last_inbound_at: None,
            sync_last_outbound_at: None,
            sync_conflict_summary: None,
            sync_conflict_at: None,
            tags: payload.tags,
            created_at: now,
            updated_at: now,
            revision: String::new(),
        });

        let body = payload.body.unwrap_or_default();
        let content = render_entity_markdown(&mut fm, &body).map_err(ServiceError::Other)?;
        atomic_write(&path, &content).map_err(ServiceError::Other)?;
        let indexed = self
            .indexer
            .index_file(&path)
            .map_err(ServiceError::Other)?;

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
        )
        .map_err(ServiceError::Other)?;

        self.db
            .read_entity_snapshot(&id)?
            .context("created note not found after indexing")
            .map_err(ServiceError::Other)
    }

    fn create_note_linked_to_project_docs(
        &self,
        project: crate::types::ProjectItem,
        payload: CreateNotePayload,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let project_id = payload
            .project_id
            .clone()
            .with_context(|| "project_id is required for project-linked notes")
            .map_err(ServiceError::Other)?;
        let now = Utc::now();
        let id = format!("{}_{}", EntityType::Note.prefix(), Ulid::new());
        let title = payload.title;
        let title_slug = slugify(&title);
        let relative_path = self.next_local_note_sync_path(&project, &title)?;
        let target_path = self.note_sync_target_path(&project, &relative_path)?;
        let body = payload.body.unwrap_or_default();
        let body_hash = compute_file_hash(&body);

        ensure_parent_dir(&target_path).map_err(ServiceError::Other)?;
        atomic_write(&target_path, &body).map_err(ServiceError::Other)?;

        let note_path = self
            .config
            .notes_dir()
            .join(&project_id)
            .join(format!("{id}-{title_slug}.md"));
        self.config
            .ensure_within_workspace(&note_path)
            .map_err(ServiceError::Other)?;

        let snapshot = self.write_note_entity(
            &note_path,
            body,
            NoteFrontmatter {
                id: id.clone(),
                entity_type: EntityType::Note,
                title: synced_note_title_from_path(&relative_path),
                project_id: Some(project_id),
                sync_kind: Some(NoteSyncKind::RepoMarkdown),
                sync_path: Some(relative_path),
                sync_status: Some(NoteSyncStatus::Live),
                sync_last_seen_hash: Some(body_hash),
                sync_last_inbound_at: None,
                sync_last_outbound_at: Some(now),
                sync_conflict_summary: None,
                sync_conflict_at: None,
                tags: payload.tags,
                created_at: now,
                updated_at: now,
                revision: String::new(),
            },
            NoteWriteContext {
                before_revision: None,
                archived: false,
                actor,
                action: "create_note",
                summary: "Created note",
            },
        )?;

        self.ensure_note_sync_watcher(&id)?;
        Ok(snapshot)
    }

    fn next_local_note_sync_path(
        &self,
        project: &crate::types::ProjectItem,
        title: &str,
    ) -> Result<String, ServiceError> {
        let mut base = slugify(title);
        if base.trim().is_empty() {
            base = "note".to_string();
        }
        let reserved_sync_file =
            normalize_managed_task_sync_file(project.task_sync_file.as_deref());

        for suffix in 0..10_000usize {
            let file_name = if suffix == 0 {
                format!("{base}.md")
            } else {
                format!("{base}-{}.md", suffix + 1)
            };
            let relative_path = format!("docs/{file_name}");
            if relative_path == reserved_sync_file {
                continue;
            }
            if self
                .find_synced_note_id(&project.id, &relative_path)?
                .is_some()
            {
                continue;
            }

            let target_path = self.note_sync_target_path(project, &relative_path)?;
            if target_path.exists() {
                continue;
            }

            return Ok(relative_path);
        }

        Err(ServiceError::Other(anyhow::anyhow!(
            "unable to allocate a unique docs markdown path for note"
        )))
    }

    pub fn list_linkable_note_files(
        &self,
        project_id: &str,
    ) -> Result<Vec<RepoMarkdownDocCandidate>, ServiceError> {
        let project = self.get_project_item(project_id)?;
        if project.source_kind != Some(crate::types::ProjectSourceKind::Local) {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "linked docs require a project with a linked local source folder"
            )));
        }

        let source_locator = project
            .source_locator
            .as_deref()
            .with_context(|| "project has no linked local source folder")
            .map_err(ServiceError::Other)?;
        let source_root = PathBuf::from(source_locator);
        if !source_root.is_dir() {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "linked source path is not a directory"
            )));
        }

        let excluded_sync_file =
            normalize_managed_task_sync_file(project.task_sync_file.as_deref());
        let mut candidates = list_repo_markdown_docs(&source_root).map_err(ServiceError::Other)?;
        candidates.retain(|candidate| candidate.relative_path != excluded_sync_file);
        Ok(candidates)
    }

    pub fn link_note_to_repo_file(
        &self,
        project_id: &str,
        relative_path: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let project = self.get_project_item(project_id)?;
        let normalized_relative_path =
            self.validate_linkable_repo_markdown_path(&project, relative_path)?;

        if let Some(existing_id) =
            self.find_synced_note_id(project_id, &normalized_relative_path)?
        {
            let note = self.load_note_state(&existing_id)?;
            if note.1.sync_status == Some(NoteSyncStatus::Live) {
                self.ensure_note_sync_watcher(&existing_id)?;
            }
            return self
                .db
                .read_entity_snapshot(&existing_id)?
                .context("synced note not found after lookup")
                .map_err(ServiceError::Other);
        }

        let target_path = self.note_sync_target_path(&project, &normalized_relative_path)?;
        let body = std::fs::read_to_string(&target_path)
            .with_context(|| format!("failed reading {}", target_path.display()))
            .map_err(ServiceError::Other)?;
        let now = Utc::now();
        let id = format!("{}_{}", EntityType::Note.prefix(), Ulid::new());
        let title = synced_note_title_from_path(&normalized_relative_path);
        let title_slug = slugify(&title);
        let note_path = self
            .config
            .notes_dir()
            .join(project_id)
            .join(format!("{id}-{title_slug}.md"));
        self.config
            .ensure_within_workspace(&note_path)
            .map_err(ServiceError::Other)?;

        let snapshot = self.write_note_entity(
            &note_path,
            body.clone(),
            NoteFrontmatter {
                id: id.clone(),
                entity_type: EntityType::Note,
                title,
                project_id: Some(project_id.to_string()),
                sync_kind: Some(NoteSyncKind::RepoMarkdown),
                sync_path: Some(normalized_relative_path.clone()),
                sync_status: Some(NoteSyncStatus::Live),
                sync_last_seen_hash: Some(compute_file_hash(&body)),
                sync_last_inbound_at: Some(now),
                sync_last_outbound_at: None,
                sync_conflict_summary: None,
                sync_conflict_at: None,
                tags: None,
                created_at: now,
                updated_at: now,
                revision: String::new(),
            },
            NoteWriteContext {
                before_revision: None,
                archived: false,
                actor,
                action: "link_note_to_repo_file",
                summary: "Linked note to repo markdown file",
            },
        )?;

        self.ensure_note_sync_watcher(&id)?;
        Ok(snapshot)
    }

    pub fn resolve_note_sync_from_file(
        &self,
        note_id: &str,
        expected_revision: &str,
    ) -> Result<EntitySnapshot, ServiceError> {
        let (_record, _frontmatter, _body, current_revision) = self.load_note_state(note_id)?;
        self.enforce_current_revision(expected_revision, &current_revision)?;
        self.import_note_sync_from_disk(note_id, true)
    }

    pub fn resolve_note_sync_from_local(
        &self,
        note_id: &str,
        expected_revision: &str,
    ) -> Result<EntitySnapshot, ServiceError> {
        let (_record, _frontmatter, _body, current_revision) = self.load_note_state(note_id)?;
        self.enforce_current_revision(expected_revision, &current_revision)?;
        self.clear_note_sync_dirty(note_id);
        self.write_note_sync_file_from_local(note_id, false)
    }

    pub fn update_task(
        &self,
        id: &str,
        patch: TaskPatch,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let requested_status = patch.status.clone();
        let updated = self
            .update_tasks(
                vec![TaskUpdateRequest {
                    id: id.to_string(),
                    expected_revision: expected_revision.to_string(),
                    patch,
                }],
                actor.clone(),
            )?
            .into_iter()
            .next()
            .context("updated task missing from batch result")
            .map_err(ServiceError::Other)?;

        if let Some(project_id) = updated.frontmatter.project_id() {
            if requested_status
                .as_ref()
                .is_some_and(|status| *status != TaskStatus::Done)
            {
                self.reactivate_section_heading_for_task(project_id, &updated.id, actor)?;
            }
            self.queue_managed_task_sync_outbound(project_id, "update_task");
        }

        Ok(updated)
    }

    fn reactivate_section_heading_for_task(
        &self,
        project_id: &str,
        task_id: &str,
        actor: Actor,
    ) -> Result<(), ServiceError> {
        let mut tasks = self.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project_id.to_string()),
            assignee: None,
            include_archived: false,
            limit: Some(UNBOUNDED_QUERY_LIMIT),
        })?;
        tasks.sort_by(|left, right| {
            effective_task_sort_order(left)
                .cmp(&effective_task_sort_order(right))
                .then(left.created_at.cmp(&right.created_at))
        });

        let Some(task_index) = tasks.iter().position(|task| task.id == task_id) else {
            return Ok(());
        };
        let reopened_task = &tasks[task_index];
        if reopened_task.status == TaskStatus::Done || is_heading_title(&reopened_task.title) {
            return Ok(());
        }

        let Some(section_heading) = tasks[..task_index]
            .iter()
            .rev()
            .find(|task| is_heading_title(&task.title))
        else {
            return Ok(());
        };
        if section_heading.status != TaskStatus::Done {
            return Ok(());
        }

        self.update_tasks(
            vec![TaskUpdateRequest {
                id: section_heading.id.clone(),
                expected_revision: section_heading.revision.clone(),
                patch: TaskPatch {
                    status: Some(TaskStatus::Todo),
                    ..Default::default()
                },
            }],
            actor,
        )?;

        Ok(())
    }

    pub fn update_tasks(
        &self,
        updates: Vec<TaskUpdateRequest>,
        actor: Actor,
    ) -> Result<Vec<EntitySnapshot>, ServiceError> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }

        let mut pending = Vec::with_capacity(updates.len());
        let mut paths = Vec::with_capacity(updates.len());

        for update in updates {
            let (record, parsed) = self
                .db
                .parse_entity_from_disk(&update.id)?
                .context("task not found")?;

            if record.entity_type != EntityType::Task {
                return Err(anyhow::anyhow!("entity {} is not a task", update.id).into());
            }

            self.enforce_revision(&update.expected_revision, &parsed)?;

            let (mut frontmatter, mut body) = split_task(parsed.frontmatter, parsed.body)?;

            let mut title_changed = false;
            if let Some(value) = update.patch.title {
                frontmatter.title = value;
                title_changed = true;
            }
            if let Some(value) = update.patch.status {
                if value == TaskStatus::Done && frontmatter.status != TaskStatus::Done {
                    frontmatter.completed_at = Some(Utc::now());
                } else if value != TaskStatus::Done && frontmatter.status == TaskStatus::Done {
                    frontmatter.completed_at = None;
                }
                frontmatter.status = value;
            }
            if let Some(value) = update.patch.priority {
                frontmatter.priority = value;
            }
            if let Some(value) = update.patch.assignee {
                frontmatter.assignee = value;
            }
            if let Some(value) = update.patch.due_at {
                frontmatter.due_at = parse_optional_datetime(&value)?;
            }
            if let Some(value) = update.patch.sort_order {
                frontmatter.sort_order = value;
            }
            if let Some(value) = update.patch.tags {
                frontmatter.tags = Some(value);
            }
            if let Some(value) = update.patch.body {
                body = value;
            }

            if title_changed
                && frontmatter.sync_managed
                && frontmatter.sync_kind == Some(TaskSyncKind::ManagedTodoFile)
            {
                frontmatter.sync_key = Some(ensure_sync_key_for_title(
                    frontmatter.sync_key.as_deref(),
                    &frontmatter.title,
                ));
            }

            frontmatter.updated_at = Utc::now();

            let mut entity = EntityFrontmatter::Task(frontmatter);
            let rendered = render_entity_markdown(&mut entity, &body)?;
            atomic_write(&record.path, &rendered)?;

            paths.push(record.path.clone());
            pending.push(PendingMutation {
                path: record.path,
                body,
                frontmatter: entity,
                before_revision: Some(record.revision),
                archived: false,
            });
        }

        let indexed = self.indexer.index_files(&paths)?;
        let mut out = Vec::with_capacity(indexed.len());
        let mut activity = Vec::with_capacity(indexed.len());
        for (pending, indexed) in pending.into_iter().zip(indexed) {
            let revision = indexed.revision.clone();
            activity.push(OwnedEntityActivityMeta {
                action: "update_task",
                entity_type: EntityType::Task,
                entity_id: pending.frontmatter.id().to_string(),
                path: pending.path.clone(),
                before_revision: pending.before_revision,
                after_revision: Some(revision.clone()),
                summary: "Updated task",
            });
            out.push(snapshot_from_parts(
                &pending.path,
                pending.body,
                pending.frontmatter,
                revision,
                pending.archived,
            ));
        }

        self.record_entity_activities(&actor, activity)?;
        Ok(out)
    }

    pub fn update_note(
        &self,
        id: &str,
        patch: NotePatch,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let (record, mut frontmatter, mut body, current_revision) = self.load_note_state(id)?;
        if current_revision != expected_revision {
            return Err(ServiceError::Conflict {
                expected: expected_revision.to_string(),
                current: current_revision,
            });
        }

        if let Some(value) = patch.title {
            if frontmatter.sync_kind.is_none() {
                frontmatter.title = value;
            }
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

        let snapshot = self.write_note_entity(
            &record.path,
            body,
            frontmatter,
            NoteWriteContext {
                before_revision: Some(record.revision.clone()),
                archived: record.archived,
                actor,
                action: "update_note",
                summary: "Updated note",
            },
        )?;

        if let EntityFrontmatter::Note(note) = &snapshot.frontmatter {
            if note.sync_kind == Some(NoteSyncKind::RepoMarkdown)
                && note.sync_status == Some(NoteSyncStatus::Live)
            {
                self.queue_note_sync_outbound(&snapshot.id);
            }
        }

        Ok(snapshot)
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
        let previous_source_kind = frontmatter.source_kind.clone();
        let previous_source_locator = frontmatter.source_locator.clone();

        if let Some(value) = patch.title {
            frontmatter.title = value;
        }
        if let Some(value) = patch.status {
            frontmatter.status = value;
        }
        if let Some(value) = patch.owner {
            frontmatter.owner = value;
        }
        if let Some(value) = patch.icon {
            frontmatter.icon = Some(value);
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
        if let Some(value) = patch.task_sync_mode {
            frontmatter.task_sync_mode = value;
        }
        if let Some(value) = patch.task_sync_file {
            frontmatter.task_sync_file = value;
        }
        if let Some(value) = patch.task_sync_enabled {
            frontmatter.task_sync_enabled = value;
        }
        if let Some(value) = patch.task_sync_status {
            frontmatter.task_sync_status = value;
        }
        if let Some(value) = patch.task_sync_last_seen_hash {
            frontmatter.task_sync_last_seen_hash = value;
        }
        if let Some(value) = patch.task_sync_last_inbound_at {
            frontmatter.task_sync_last_inbound_at = value;
        }
        if let Some(value) = patch.task_sync_last_outbound_at {
            frontmatter.task_sync_last_outbound_at = value;
        }
        if let Some(value) = patch.task_sync_conflict_summary {
            frontmatter.task_sync_conflict_summary = value;
        }
        if let Some(value) = patch.task_sync_conflict_at {
            frontmatter.task_sync_conflict_at = value;
        }
        if let Some(value) = patch.tags {
            frontmatter.tags = Some(value);
        }
        if let Some(value) = patch.body {
            body = value;
        }

        let source_link_changed = frontmatter.source_kind != previous_source_kind
            || frontmatter.source_locator != previous_source_locator;
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

        let snapshot = self
            .db
            .read_entity_snapshot(id)?
            .context("updated project not found after indexing")
            .map_err(ServiceError::Other)?;

        if let Ok(project) = self.get_project_item(id) {
            if project.task_sync_enabled
                && project.task_sync_mode == Some(TaskSyncMode::ManagedTodoFile)
                && project.task_sync_status == Some(TaskSyncStatus::Live)
            {
                if let Err(err) = self.ensure_managed_task_sync_watcher(&project) {
                    self.clear_managed_task_sync_watcher(id);
                    let _ = self.pause_managed_task_sync_for_error(id, &err.to_string());
                }
            } else {
                self.clear_managed_task_sync_watcher(id);
            }
        }
        if source_link_changed {
            let _ = self.reconcile_project_note_sync_watchers(id);
        }

        Ok(snapshot)
    }

    pub fn sync_project_from_source(
        &self,
        project_id: &str,
        actor: Actor,
    ) -> Result<ProjectSyncReport, ServiceError> {
        let project = self.get_project_item(project_id)?;

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
            limit: Some(UNBOUNDED_QUERY_LIMIT),
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
        let mut pending_creates = Vec::new();
        let mut pending_updates = Vec::new();

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
                    if !pending_creates.is_empty() {
                        created += self
                            .create_tasks(std::mem::take(&mut pending_creates), actor.clone())?
                            .len();
                    }

                    pending_updates.push(TaskUpdateRequest {
                        id: existing.id.clone(),
                        expected_revision: existing.revision.clone(),
                        patch,
                    });
                }
                continue;
            }

            if !pending_updates.is_empty() {
                updated += self
                    .update_tasks(std::mem::take(&mut pending_updates), actor.clone())?
                    .len();
            }

            pending_creates.push(CreateTaskPayload {
                title: candidate.title.clone(),
                project_id: project_id.to_string(),
                status: Some(desired_status),
                priority: Some(TaskPriority::P0),
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
            });
        }

        if !pending_updates.is_empty() {
            updated += self.update_tasks(pending_updates, actor.clone())?.len();
        }
        if !pending_creates.is_empty() {
            created += self.create_tasks(pending_creates, actor.clone())?.len();
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

        self.update_project(
            project_id,
            ProjectPatch {
                sync_source_key: Some(Some(source_key)),
                last_synced_at: Some(Some(synced_at)),
                last_sync_summary: Some(Some(summary.clone())),
                ..ProjectPatch::default()
            },
            &project.revision,
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

    pub fn enable_managed_task_sync(
        &self,
        project_id: &str,
        expected_revision: &str,
    ) -> Result<crate::types::ProjectItem, ServiceError> {
        let project = self.get_project_item(project_id)?;
        self.enforce_current_revision(expected_revision, &project.revision)?;
        if project.source_kind != Some(crate::types::ProjectSourceKind::Local) {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "managed task sync requires a linked local source folder"
            )));
        }

        let source_locator = project
            .source_locator
            .clone()
            .with_context(|| "project has no linked local source folder")
            .map_err(ServiceError::Other)?;
        let source_root = PathBuf::from(&source_locator);
        if !source_root.is_dir() {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "linked source path is not a directory"
            )));
        }

        let sync_file = normalize_managed_task_sync_file(project.task_sync_file.as_deref());

        let updated = self
            .update_project(
                project_id,
                ProjectPatch {
                    task_sync_mode: Some(Some(TaskSyncMode::ManagedTodoFile)),
                    task_sync_file: Some(Some(sync_file.clone())),
                    task_sync_enabled: Some(true),
                    task_sync_status: Some(Some(TaskSyncStatus::Paused)),
                    task_sync_conflict_summary: Some(None),
                    task_sync_conflict_at: Some(None),
                    ..ProjectPatch::default()
                },
                &project.revision,
                Actor::human("operator"),
            )
            .and_then(|_| self.get_project_item(project_id))?;

        let setup_result = (|| -> Result<crate::types::ProjectItem, ServiceError> {
            self.ensure_tasks_marked_for_managed_sync(project_id, &sync_file)?;
            let sync_path = self.managed_task_sync_file_path(&updated)?;
            if sync_path.exists() {
                self.import_managed_task_sync_from_disk(project_id, false)?;
            } else {
                let live_project = self.get_project_item(project_id)?;
                self.write_managed_task_sync_file_from_local(&live_project, false)?;
            }
            let live_project = self.get_project_item(project_id)?;
            self.ensure_managed_task_sync_watcher(&live_project)?;
            self.record_project_sync_activity(
                Actor::human("operator"),
                project_id,
                "enable_task_sync",
                "Enabled managed task sync",
            )?;
            self.get_project_item(project_id)
        })();

        if let Err(err) = &setup_result {
            let _ = self.pause_managed_task_sync_for_error(project_id, &err.to_string());
        }

        setup_result
    }

    pub fn pause_managed_task_sync(
        &self,
        project_id: &str,
        expected_revision: &str,
    ) -> Result<crate::types::ProjectItem, ServiceError> {
        let project = self.get_project_item(project_id)?;
        self.enforce_current_revision(expected_revision, &project.revision)?;
        self.update_project(
            project_id,
            ProjectPatch {
                task_sync_status: Some(Some(TaskSyncStatus::Paused)),
                ..ProjectPatch::default()
            },
            expected_revision,
            Actor::human("operator"),
        )?;
        self.clear_managed_task_sync_dirty(project_id);
        self.record_project_sync_activity(
            Actor::human("operator"),
            project_id,
            "pause_task_sync",
            "Paused managed task sync",
        )?;
        self.get_project_item(project_id)
    }

    pub fn resume_managed_task_sync(
        &self,
        project_id: &str,
        expected_revision: &str,
    ) -> Result<crate::types::ProjectItem, ServiceError> {
        let project = self.get_project_item(project_id)?;
        self.enforce_current_revision(expected_revision, &project.revision)?;

        self.clear_managed_task_sync_dirty(project_id);
        let sync_path = self.managed_task_sync_file_path(&project)?;
        let current_hash = compute_file_hash_from_path(&sync_path).map_err(ServiceError::Other)?;
        self.update_project(
            project_id,
            ProjectPatch {
                task_sync_status: Some(Some(TaskSyncStatus::Paused)),
                task_sync_last_seen_hash: Some(current_hash),
                task_sync_conflict_summary: Some(None),
                task_sync_conflict_at: Some(None),
                ..ProjectPatch::default()
            },
            expected_revision,
            Actor::human("operator"),
        )?;
        let resume_result = (|| -> Result<crate::types::ProjectItem, ServiceError> {
            let live_project = self.get_project_item(project_id)?;
            self.ensure_managed_task_sync_watcher(&live_project)?;
            self.import_managed_task_sync_from_disk(project_id, false)?;
            let current_project = self.get_project_item(project_id)?;
            if current_project.task_sync_status != Some(TaskSyncStatus::Live) {
                self.update_project(
                    project_id,
                    ProjectPatch {
                        task_sync_status: Some(Some(TaskSyncStatus::Live)),
                        task_sync_conflict_summary: Some(None),
                        task_sync_conflict_at: Some(None),
                        ..ProjectPatch::default()
                    },
                    &current_project.revision,
                    Actor::human("operator"),
                )?;
            }
            self.record_project_sync_activity(
                Actor::human("operator"),
                project_id,
                "resume_task_sync",
                "Resumed managed task sync",
            )?;
            self.get_project_item(project_id)
        })();

        if let Err(err) = &resume_result {
            let _ = self.pause_managed_task_sync_for_error(project_id, &err.to_string());
        }

        resume_result
    }

    pub fn resolve_managed_task_sync_from_file(
        &self,
        project_id: &str,
        expected_revision: &str,
    ) -> Result<crate::types::ProjectItem, ServiceError> {
        let project = self.get_project_item(project_id)?;
        self.enforce_current_revision(expected_revision, &project.revision)?;

        self.clear_managed_task_sync_dirty(project_id);
        self.import_managed_task_sync_from_disk(project_id, true)?;
        self.record_project_sync_activity(
            Actor::human("operator"),
            project_id,
            "resolve_task_sync_from_file",
            "Resolved managed task sync using sync file",
        )?;
        self.get_project_item(project_id)
    }

    pub fn resolve_managed_task_sync_from_local(
        &self,
        project_id: &str,
        expected_revision: &str,
    ) -> Result<crate::types::ProjectItem, ServiceError> {
        let project = self.get_project_item(project_id)?;
        self.enforce_current_revision(expected_revision, &project.revision)?;

        self.clear_managed_task_sync_dirty(project_id);
        self.write_managed_task_sync_file_from_local(&project, false)?;
        self.record_project_sync_activity(
            Actor::human("operator"),
            project_id,
            "resolve_task_sync_from_local",
            "Resolved managed task sync using Topside state",
        )?;
        self.get_project_item(project_id)
    }

    pub fn archive_entity(
        &self,
        id: &str,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let record = self
            .db
            .get_entity_record(id)?
            .context("entity not found")
            .map_err(ServiceError::Other)?;

        if record.entity_type == EntityType::Project {
            return self.archive_project_entity(record, expected_revision, actor);
        }

        let archived = self
            .archive_entities(
                vec![ArchiveEntityRequest {
                    id: id.to_string(),
                    expected_revision: expected_revision.to_string(),
                }],
                actor,
            )?
            .into_iter()
            .next()
            .context("archived entity missing from batch result")
            .map_err(ServiceError::Other)?;

        if archived.entity_type == EntityType::Task {
            if let Some(project_id) = archived.frontmatter.project_id() {
                self.queue_managed_task_sync_outbound(project_id, "archive_task");
            }
        } else if archived.entity_type == EntityType::Note {
            self.clear_note_sync_runtime(&archived.id);
        }

        Ok(archived)
    }

    pub fn archive_tasks(
        &self,
        requests: Vec<ArchiveEntityRequest>,
        actor: Actor,
    ) -> Result<Vec<EntitySnapshot>, ServiceError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }

        for request in &requests {
            let record = self
                .db
                .get_entity_record(&request.id)?
                .context("task not found")
                .map_err(ServiceError::Other)?;

            if record.entity_type != EntityType::Task {
                return Err(anyhow::anyhow!("entity {} is not a task", request.id).into());
            }
        }

        let archived = self.archive_entities(requests, actor)?;
        let mut project_ids = HashSet::new();
        for entity in &archived {
            if let Some(project_id) = entity.frontmatter.project_id() {
                project_ids.insert(project_id.to_string());
            }
        }

        for project_id in project_ids {
            self.queue_managed_task_sync_outbound(&project_id, "archive_tasks");
        }

        Ok(archived)
    }

    pub fn restore_entity(
        &self,
        id: &str,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let record = self
            .db
            .get_entity_record(id)?
            .context("entity not found")
            .map_err(ServiceError::Other)?;
        if !record.archived {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "entity is not archived"
            )));
        }

        if record.entity_type == EntityType::Project {
            return self.restore_project_entity(record, expected_revision, actor);
        }

        let restored = self
            .restore_entities(
                vec![RestoreEntityRequest {
                    id: id.to_string(),
                    expected_revision: expected_revision.to_string(),
                }],
                actor,
            )?
            .into_iter()
            .next()
            .context("restored entity missing from batch result")
            .map_err(ServiceError::Other)?;

        self.finish_restored_entity(&restored);
        Ok(restored)
    }

    fn archive_project_entity(
        &self,
        record: StoredEntityRecord,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let tasks = self.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(record.id.clone()),
            assignee: None,
            include_archived: false,
            limit: Some(UNBOUNDED_QUERY_LIMIT),
        })?;
        let notes = self
            .db
            .list_note_details_for_project(&record.id, UNBOUNDED_QUERY_LIMIT, false)
            .map_err(ServiceError::Other)?;

        let mut requests = Vec::with_capacity(1 + tasks.len() + notes.len());
        requests.push(ArchiveEntityRequest {
            id: record.id.clone(),
            expected_revision: expected_revision.to_string(),
        });
        requests.extend(tasks.iter().map(|task| ArchiveEntityRequest {
            id: task.id.clone(),
            expected_revision: task.revision.clone(),
        }));
        requests.extend(notes.iter().map(|note| ArchiveEntityRequest {
            id: note.id.clone(),
            expected_revision: note.revision.clone(),
        }));

        let archived = self.archive_entities(requests, actor)?;

        self.clear_managed_task_sync_dirty(&record.id);
        self.clear_managed_task_sync_dirty_flag(&record.id);
        self.clear_managed_task_sync_watcher(&record.id);
        for note in notes {
            self.clear_note_sync_runtime(&note.id);
        }

        archived
            .into_iter()
            .find(|entity| entity.id == record.id)
            .context("archived project missing from batch result")
            .map_err(ServiceError::Other)
    }

    fn restore_project_entity(
        &self,
        record: StoredEntityRecord,
        expected_revision: &str,
        actor: Actor,
    ) -> Result<EntitySnapshot, ServiceError> {
        let tasks = self
            .list_tasks(&TaskFilters {
                status: None,
                priority: None,
                project_id: Some(record.id.clone()),
                assignee: None,
                include_archived: true,
                limit: Some(UNBOUNDED_QUERY_LIMIT),
            })?
            .into_iter()
            .filter(|task| task.archived)
            .collect::<Vec<_>>();
        let notes = self
            .db
            .list_note_details_for_project(&record.id, UNBOUNDED_QUERY_LIMIT, true)
            .map_err(ServiceError::Other)?
            .into_iter()
            .filter(|note| note.archived)
            .collect::<Vec<_>>();

        let restored_project = self
            .restore_entities(
                vec![RestoreEntityRequest {
                    id: record.id.clone(),
                    expected_revision: expected_revision.to_string(),
                }],
                actor.clone(),
            )?
            .into_iter()
            .next()
            .context("restored project missing from batch result")
            .map_err(ServiceError::Other)?;

        let mut child_requests = Vec::with_capacity(tasks.len() + notes.len());
        child_requests.extend(tasks.into_iter().map(|task| RestoreEntityRequest {
            id: task.id,
            expected_revision: task.revision,
        }));
        child_requests.extend(notes.into_iter().map(|note| RestoreEntityRequest {
            id: note.id,
            expected_revision: note.revision,
        }));

        if !child_requests.is_empty() {
            self.restore_entities(child_requests, actor)?;
        }

        self.finish_restored_project(&record.id);
        Ok(restored_project)
    }

    pub fn archive_entities(
        &self,
        requests: Vec<ArchiveEntityRequest>,
        actor: Actor,
    ) -> Result<Vec<EntitySnapshot>, ServiceError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }

        let mut pending = Vec::with_capacity(requests.len());
        let mut removed_paths = Vec::with_capacity(requests.len());
        let mut target_paths = Vec::with_capacity(requests.len());

        for request in requests {
            let record = self
                .db
                .get_entity_record(&request.id)?
                .context("entity not found")?;

            let raw = std::fs::read_to_string(&record.path)
                .with_context(|| format!("failed reading {}", record.path.display()))?;
            let parsed = parse_entity_markdown(&raw)?;
            self.enforce_revision(&request.expected_revision, &parsed)?;
            let ParsedEntity {
                frontmatter,
                body,
                revision: _,
                links: _,
            } = parsed;

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

            removed_paths.push(record.path);
            target_paths.push(target.clone());
            pending.push(PendingMutation {
                path: target,
                body,
                frontmatter,
                before_revision: Some(record.revision),
                archived: true,
            });
        }

        self.db.remove_paths(&removed_paths)?;
        let indexed = self.indexer.index_files(&target_paths)?;
        let mut archived = Vec::with_capacity(indexed.len());
        let mut activity = Vec::with_capacity(indexed.len());
        for (pending, indexed) in pending.into_iter().zip(indexed) {
            let revision = indexed.revision.clone();
            activity.push(OwnedEntityActivityMeta {
                action: "archive_entity",
                entity_type: indexed.entity_type,
                entity_id: pending.frontmatter.id().to_string(),
                path: pending.path.clone(),
                before_revision: pending.before_revision,
                after_revision: Some(revision.clone()),
                summary: "Archived entity",
            });
            archived.push(snapshot_from_parts(
                &pending.path,
                pending.body,
                pending.frontmatter,
                revision,
                pending.archived,
            ));
        }

        self.record_entity_activities(&actor, activity)?;
        Ok(archived)
    }

    pub fn restore_entities(
        &self,
        requests: Vec<RestoreEntityRequest>,
        actor: Actor,
    ) -> Result<Vec<EntitySnapshot>, ServiceError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }

        let mut pending = Vec::with_capacity(requests.len());
        let mut removed_paths = Vec::with_capacity(requests.len());
        let mut target_paths = Vec::with_capacity(requests.len());

        for request in requests {
            let record = self
                .db
                .get_entity_record(&request.id)?
                .context("entity not found")?;
            if !record.archived {
                return Err(ServiceError::Other(anyhow::anyhow!(
                    "entity {} is not archived",
                    request.id
                )));
            }

            let raw = std::fs::read_to_string(&record.path)
                .with_context(|| format!("failed reading {}", record.path.display()))?;
            let parsed = parse_entity_markdown(&raw)?;
            self.enforce_revision(&request.expected_revision, &parsed)?;
            let ParsedEntity {
                frontmatter,
                body,
                revision: _,
                links: _,
            } = parsed;

            let mut target = self.restore_target_path(&frontmatter)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed creating {}", parent.display()))?;
            }

            let file_name = target
                .file_name()
                .and_then(|value| value.to_str())
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("{}-{}.md", record.id, Ulid::new()));
            if target.exists() {
                let parent = target
                    .parent()
                    .map(Path::to_path_buf)
                    .context("restore target had no parent")
                    .map_err(ServiceError::Other)?;
                target = parent.join(format!("{}-{}", Ulid::new(), file_name));
            }

            self.config.ensure_within_workspace(&target)?;
            std::fs::rename(&record.path, &target).with_context(|| {
                format!(
                    "failed moving {} to {}",
                    record.path.display(),
                    target.display()
                )
            })?;

            removed_paths.push(record.path);
            target_paths.push(target.clone());
            pending.push(PendingMutation {
                path: target,
                body,
                frontmatter,
                before_revision: Some(record.revision),
                archived: false,
            });
        }

        self.db.remove_paths(&removed_paths)?;
        let indexed = self.indexer.index_files(&target_paths)?;
        let mut restored = Vec::with_capacity(indexed.len());
        let mut activity = Vec::with_capacity(indexed.len());
        for (pending, indexed) in pending.into_iter().zip(indexed) {
            let revision = indexed.revision.clone();
            activity.push(OwnedEntityActivityMeta {
                action: "restore_entity",
                entity_type: indexed.entity_type,
                entity_id: pending.frontmatter.id().to_string(),
                path: pending.path.clone(),
                before_revision: pending.before_revision,
                after_revision: Some(revision.clone()),
                summary: "Restored entity",
            });
            restored.push(snapshot_from_parts(
                &pending.path,
                pending.body,
                pending.frontmatter,
                revision,
                pending.archived,
            ));
        }

        self.record_entity_activities(&actor, activity)?;
        Ok(restored)
    }

    pub fn empty_archive(&self) -> Result<usize, ServiceError> {
        let archived_projects = self
            .list_projects(UNBOUNDED_QUERY_LIMIT, true)?
            .into_iter()
            .filter(|project| project.archived)
            .map(|project| PathBuf::from(project.path));
        let archived_tasks = self
            .list_tasks(&TaskFilters {
                status: None,
                priority: None,
                project_id: None,
                assignee: None,
                include_archived: true,
                limit: Some(UNBOUNDED_QUERY_LIMIT),
            })?
            .into_iter()
            .filter(|task| task.archived)
            .map(|task| PathBuf::from(task.path));
        let archived_notes = self
            .list_notes(UNBOUNDED_QUERY_LIMIT, true)?
            .into_iter()
            .filter(|note| note.archived)
            .map(|note| PathBuf::from(note.path));

        let paths = archived_projects
            .chain(archived_tasks)
            .chain(archived_notes)
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return Ok(0);
        }

        for path in &paths {
            if path.exists() {
                std::fs::remove_file(path)
                    .with_context(|| format!("failed removing {}", path.display()))
                    .map_err(ServiceError::Other)?;
            }
        }

        self.db.remove_paths(&paths)?;
        Ok(paths.len())
    }

    fn next_task_sort_order(&self, project_id: &str) -> Result<i64, ServiceError> {
        let tasks = self.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project_id.to_string()),
            assignee: None,
            include_archived: false,
            limit: Some(UNBOUNDED_QUERY_LIMIT),
        })?;
        let next = tasks
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

        let current_sort_orders = workspace
            .active_tasks
            .iter()
            .map(|task| (task.id.clone(), effective_task_sort_order(task)))
            .collect::<std::collections::HashMap<_, _>>();
        let now = Utc::now();
        for (index, task_id) in ordered_active_task_ids.iter().enumerate() {
            let desired_sort_order = (index as i64) + 1;
            if current_sort_orders.get(task_id).copied() == Some(desired_sort_order) {
                continue;
            }

            let (record, parsed) = self
                .db
                .parse_entity_from_disk(task_id)?
                .context("task not found during reorder")?;
            let (mut frontmatter, body) = split_task(parsed.frontmatter, parsed.body)?;
            frontmatter.sort_order = desired_sort_order;
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

    fn get_project_item(
        &self,
        project_id: &str,
    ) -> Result<crate::types::ProjectItem, ServiceError> {
        self.list_projects(UNBOUNDED_QUERY_LIMIT, false)?
            .into_iter()
            .find(|item| item.id == project_id)
            .with_context(|| format!("project {project_id} not found"))
            .map_err(ServiceError::Other)
    }

    fn codex_session_store(&self) -> CodexSessionStore {
        CodexSessionStore::new(self.config.clone())
    }

    fn restore_target_path(
        &self,
        frontmatter: &EntityFrontmatter,
    ) -> Result<PathBuf, ServiceError> {
        match frontmatter {
            EntityFrontmatter::Project(project) => Ok(self.config.projects_dir().join(format!(
                "{}-{}.md",
                project.id,
                slugify(&project.title)
            ))),
            EntityFrontmatter::Task(task) => {
                self.get_project_item(&task.project_id)?;
                Ok(self.config.tasks_dir().join(&task.project_id).join(format!(
                    "{}-{}.md",
                    task.id,
                    slugify(&task.title)
                )))
            }
            EntityFrontmatter::Note(note) => {
                let base_dir = match note.project_id.as_deref() {
                    Some(project_id) => {
                        self.get_project_item(project_id)?;
                        self.config.notes_dir().join(project_id)
                    }
                    None => self.config.notes_dir().join("inbox"),
                };

                Ok(base_dir.join(format!("{}-{}.md", note.id, slugify(&note.title))))
            }
        }
    }

    fn finish_restored_entity(&self, restored: &EntitySnapshot) {
        match &restored.frontmatter {
            EntityFrontmatter::Task(task) => {
                self.queue_managed_task_sync_outbound(&task.project_id, "restore_task");
            }
            EntityFrontmatter::Note(note) => {
                if note.sync_kind == Some(NoteSyncKind::RepoMarkdown)
                    && note.sync_status == Some(NoteSyncStatus::Live)
                {
                    if let Some(project_id) = note.project_id.as_deref() {
                        let _ = self.reconcile_project_note_sync_watchers(project_id);
                    }
                }
            }
            EntityFrontmatter::Project(_) => self.finish_restored_project(&restored.id),
        }
    }

    fn finish_restored_project(&self, project_id: &str) {
        let Ok(project) = self.get_project_item(project_id) else {
            return;
        };

        if project.task_sync_enabled
            && project.task_sync_mode == Some(TaskSyncMode::ManagedTodoFile)
            && project.task_sync_status == Some(TaskSyncStatus::Live)
        {
            if let Err(err) = self.ensure_managed_task_sync_watcher(&project) {
                self.clear_managed_task_sync_watcher(project_id);
                let _ = self.pause_managed_task_sync_for_error(project_id, &err.to_string());
            } else {
                self.queue_managed_task_sync_outbound(project_id, "restore_project");
            }
        }

        let _ = self.reconcile_project_note_sync_watchers(project_id);
    }

    fn managed_task_sync_defaults_for_new_task(
        &self,
        project_id: &str,
        title: &str,
    ) -> Result<ManagedTaskSyncDefaults, anyhow::Error> {
        let project = self
            .list_projects(UNBOUNDED_QUERY_LIMIT, false)?
            .into_iter()
            .find(|item| item.id == project_id);
        let Some(project) = project else {
            return Ok((None, None, None, None));
        };

        if !project.task_sync_enabled
            || project.task_sync_mode != Some(TaskSyncMode::ManagedTodoFile)
            || project.source_kind != Some(crate::types::ProjectSourceKind::Local)
        {
            return Ok((None, None, None, None));
        }

        let sync_path = normalize_managed_task_sync_file(project.task_sync_file.as_deref());

        Ok((
            Some(TaskSyncKind::ManagedTodoFile),
            Some(sync_path),
            Some(ensure_sync_key_for_title(None, title)),
            Some(true),
        ))
    }

    fn load_note_state(
        &self,
        note_id: &str,
    ) -> Result<(StoredEntityRecord, NoteFrontmatter, String, String), ServiceError> {
        let (record, parsed) = self
            .db
            .parse_entity_from_disk(note_id)?
            .context("note not found")
            .map_err(ServiceError::Other)?;
        if record.entity_type != EntityType::Note {
            return Err(anyhow::anyhow!("entity {note_id} is not a note").into());
        }

        let current_revision = parsed.revision.clone();
        let (frontmatter, body) =
            split_note(parsed.frontmatter, parsed.body).map_err(ServiceError::Other)?;
        Ok((record, frontmatter, body, current_revision))
    }

    fn write_note_entity(
        &self,
        path: &Path,
        body: String,
        frontmatter: NoteFrontmatter,
        context: NoteWriteContext,
    ) -> Result<EntitySnapshot, ServiceError> {
        let mut entity = EntityFrontmatter::Note(frontmatter);
        let rendered = render_entity_markdown(&mut entity, &body).map_err(ServiceError::Other)?;
        atomic_write(path, &rendered).map_err(ServiceError::Other)?;
        let indexed = self.indexer.index_file(path).map_err(ServiceError::Other)?;
        let revision = indexed.revision.clone();

        self.record_entity_activity(
            context.actor,
            EntityActivityMeta {
                action: context.action,
                entity_type: EntityType::Note,
                entity_id: entity.id(),
                path,
                before_revision: context.before_revision,
                after_revision: Some(revision.clone()),
                summary: context.summary,
            },
        )
        .map_err(ServiceError::Other)?;

        Ok(snapshot_from_parts(
            path,
            body,
            entity,
            revision,
            context.archived,
        ))
    }

    fn find_synced_note_id(
        &self,
        project_id: &str,
        sync_path: &str,
    ) -> Result<Option<String>, ServiceError> {
        self.db
            .find_repo_markdown_note_id(project_id, sync_path)
            .map_err(ServiceError::Other)
    }

    fn validate_linkable_repo_markdown_path(
        &self,
        project: &crate::types::ProjectItem,
        relative_path: &str,
    ) -> Result<String, ServiceError> {
        let normalized = relative_path
            .trim()
            .replace('\\', "/")
            .trim_start_matches('/')
            .to_string();
        if normalized.is_empty() {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "relative_path is required"
            )));
        }
        if !normalized.starts_with("docs/") || !normalized.to_ascii_lowercase().ends_with(".md") {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "linked docs must be markdown files under docs/"
            )));
        }

        let excluded_sync_file =
            normalize_managed_task_sync_file(project.task_sync_file.as_deref());
        if normalized == excluded_sync_file {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "the managed task sync file cannot also be linked as a note"
            )));
        }

        let target_path = self.note_sync_target_path(project, &normalized)?;
        if !target_path.is_file() {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "selected markdown file does not exist"
            )));
        }

        Ok(normalized)
    }

    fn note_sync_target_path(
        &self,
        project: &crate::types::ProjectItem,
        relative_path: &str,
    ) -> Result<PathBuf, ServiceError> {
        if project.source_kind != Some(crate::types::ProjectSourceKind::Local) {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "linked docs require a project with a linked local source folder"
            )));
        }

        let source_locator = project
            .source_locator
            .as_deref()
            .with_context(|| "project has no linked local source folder")
            .map_err(ServiceError::Other)?;
        let source_root = PathBuf::from(source_locator);
        if !source_root.is_dir() {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "linked source path is not a directory"
            )));
        }

        resolve_managed_file_path(source_locator, relative_path).map_err(ServiceError::Other)
    }

    fn note_sync_target_path_from_frontmatter(
        &self,
        frontmatter: &NoteFrontmatter,
    ) -> Result<PathBuf, ServiceError> {
        let project_id = frontmatter
            .project_id
            .as_deref()
            .with_context(|| "synced note is missing project_id")
            .map_err(ServiceError::Other)?;
        let sync_path = frontmatter
            .sync_path
            .as_deref()
            .with_context(|| "synced note is missing sync_path")
            .map_err(ServiceError::Other)?;
        let project = self.get_project_item(project_id)?;
        self.note_sync_target_path(&project, sync_path)
    }

    fn reconcile_project_note_sync_watchers(&self, project_id: &str) -> Result<(), ServiceError> {
        let note_ids = self
            .db
            .list_repo_markdown_note_ids_for_project(project_id)
            .map_err(ServiceError::Other)?;

        for note_id in note_ids {
            self.clear_note_sync_runtime(&note_id);
            let Ok((_record, frontmatter, _body, _current_revision)) =
                self.load_note_state(&note_id)
            else {
                continue;
            };
            if frontmatter.sync_status != Some(NoteSyncStatus::Live) {
                continue;
            }

            if let Err(err) = self.ensure_note_sync_watcher(&note_id) {
                let _ = self.mark_note_sync_conflict(
                    &note_id,
                    &format!("Linked note sync watcher paused: {err}"),
                );
                continue;
            }

            let _ = self.import_note_sync_from_disk(&note_id, false);
        }

        Ok(())
    }

    fn restore_note_sync_watchers(&self) -> Result<()> {
        for note in self.list_notes(UNBOUNDED_QUERY_LIMIT, false)? {
            if note.sync_kind == Some(NoteSyncKind::RepoMarkdown)
                && note.sync_status == Some(NoteSyncStatus::Live)
            {
                if let Err(err) = self.ensure_note_sync_watcher(&note.id) {
                    let _ = self.mark_note_sync_conflict(
                        &note.id,
                        &format!("Linked note sync watcher paused: {err}"),
                    );
                }
            }
        }
        Ok(())
    }

    fn ensure_note_sync_watcher(&self, note_id: &str) -> Result<(), ServiceError> {
        let (_record, frontmatter, _body, _current_revision) = self.load_note_state(note_id)?;
        if frontmatter.sync_kind != Some(NoteSyncKind::RepoMarkdown)
            || frontmatter.sync_status != Some(NoteSyncStatus::Live)
        {
            return Ok(());
        }

        let target_path = self.note_sync_target_path_from_frontmatter(&frontmatter)?;
        let parent = target_path
            .parent()
            .map(Path::to_path_buf)
            .with_context(|| "linked markdown file has no parent directory")
            .map_err(ServiceError::Other)?;

        {
            let mut runtime = self.note_sync_runtime.lock().map_err(|_| {
                ServiceError::Other(anyhow::anyhow!("note sync runtime mutex poisoned"))
            })?;
            let state = runtime.notes.entry(note_id.to_string()).or_default();
            if state.watcher_path.as_ref() == Some(&target_path) && state.watcher.is_some() {
                return Ok(());
            }
            state.watcher = None;
            state.watcher_path = Some(target_path.clone());
        }

        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .map_err(|err| ServiceError::Other(err.into()))?;
        watcher
            .watch(&parent, RecursiveMode::NonRecursive)
            .map_err(|err| ServiceError::Other(err.into()))?;

        let service = self.clone();
        let note_id_key = note_id.to_string();
        let note_id_for_thread = note_id_key.clone();
        let target_for_thread = target_path.clone();
        let debounce = Duration::from_millis(WATCHER_DEBOUNCE_MS);
        let thread = std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut touched_target = watch_event_hits_target(first, &target_for_thread);

                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(next) => {
                            touched_target =
                                touched_target || watch_event_hits_target(next, &target_for_thread);
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }

                if touched_target {
                    let _ = service.handle_note_sync_file_event(&note_id_for_thread);
                }
            }
        });

        let mut runtime = self.note_sync_runtime.lock().map_err(|_| {
            ServiceError::Other(anyhow::anyhow!("note sync runtime mutex poisoned"))
        })?;
        let state = runtime.notes.entry(note_id_key).or_default();
        if state.watcher_path.as_ref() == Some(&target_path) {
            state.watcher = Some(ManagedNoteSyncWatcherRuntime {
                _watcher: watcher,
                _thread: thread,
            });
        }

        Ok(())
    }

    fn clear_note_sync_dirty(&self, note_id: &str) {
        if let Ok(mut runtime) = self.note_sync_runtime.lock() {
            if let Some(state) = runtime.notes.get_mut(note_id) {
                state.dirty_outbound = false;
                state.last_outbound_hash = None;
            }
        }
    }

    fn clear_note_sync_dirty_flag(&self, note_id: &str) {
        if let Ok(mut runtime) = self.note_sync_runtime.lock() {
            if let Some(state) = runtime.notes.get_mut(note_id) {
                state.dirty_outbound = false;
            }
        }
    }

    fn clear_note_sync_runtime(&self, note_id: &str) {
        if let Ok(mut runtime) = self.note_sync_runtime.lock() {
            runtime.notes.remove(note_id);
        }
    }

    fn queue_note_sync_outbound(&self, note_id: &str) {
        let nonce = {
            let Ok(mut runtime) = self.note_sync_runtime.lock() else {
                return;
            };
            let state = runtime.notes.entry(note_id.to_string()).or_default();
            state.dirty_outbound = true;
            state.outbound_nonce = state.outbound_nonce.saturating_add(1);
            state.outbound_nonce
        };

        let service = self.clone();
        let note_id = note_id.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(OUTBOUND_DEBOUNCE_MS));
            let _ = service.flush_note_sync_outbound(&note_id, nonce);
        });
    }

    fn flush_note_sync_outbound(&self, note_id: &str, nonce: u64) -> Result<(), ServiceError> {
        let should_flush = {
            let runtime = self.note_sync_runtime.lock().map_err(|_| {
                ServiceError::Other(anyhow::anyhow!("note sync runtime mutex poisoned"))
            })?;
            runtime
                .notes
                .get(note_id)
                .map(|state| state.dirty_outbound && state.outbound_nonce == nonce)
                .unwrap_or(false)
        };
        if !should_flush {
            return Ok(());
        }

        let result = match self.load_note_state(note_id) {
            Ok((_record, frontmatter, _body, _current_revision)) => {
                if frontmatter.sync_kind != Some(NoteSyncKind::RepoMarkdown)
                    || frontmatter.sync_status != Some(NoteSyncStatus::Live)
                {
                    self.clear_note_sync_dirty_flag(note_id);
                    return Ok(());
                }
                self.write_note_sync_file_from_local(note_id, true)
                    .map(|_| ())
            }
            Err(err) => {
                self.clear_note_sync_dirty(note_id);
                Err(err)
            }
        };

        self.clear_note_sync_dirty_flag(note_id);
        result
    }

    fn handle_note_sync_file_event(&self, note_id: &str) -> Result<(), ServiceError> {
        let (_record, frontmatter, _body, _current_revision) = self.load_note_state(note_id)?;
        if frontmatter.sync_kind != Some(NoteSyncKind::RepoMarkdown)
            || frontmatter.sync_status != Some(NoteSyncStatus::Live)
        {
            return Ok(());
        }

        let target_path = self.note_sync_target_path_from_frontmatter(&frontmatter)?;
        let current_hash =
            compute_file_hash_from_path(&target_path).map_err(ServiceError::Other)?;
        let (dirty_outbound, last_outbound_hash) = self
            .note_sync_runtime
            .lock()
            .map_err(|_| ServiceError::Other(anyhow::anyhow!("note sync runtime mutex poisoned")))?
            .notes
            .get(note_id)
            .map(|state| (state.dirty_outbound, state.last_outbound_hash.clone()))
            .unwrap_or((false, None));

        if let (Some(current_hash), Some(outbound_hash)) =
            (current_hash.as_deref(), last_outbound_hash.as_deref())
        {
            if current_hash == outbound_hash {
                return Ok(());
            }
        }

        if dirty_outbound {
            self.mark_note_sync_conflict(
                note_id,
                "Linked note sync detected local and file edits before reconciliation.",
            )?;
            return Ok(());
        }

        self.import_note_sync_from_disk(note_id, false).map(|_| ())
    }

    fn import_note_sync_from_disk(
        &self,
        note_id: &str,
        force: bool,
    ) -> Result<EntitySnapshot, ServiceError> {
        let (record, mut frontmatter, _body, current_revision) = self.load_note_state(note_id)?;
        if frontmatter.sync_kind != Some(NoteSyncKind::RepoMarkdown) {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "note is not linked to a repo markdown file"
            )));
        }

        let target_path = self.note_sync_target_path_from_frontmatter(&frontmatter)?;
        let raw = match std::fs::read_to_string(&target_path) {
            Ok(raw) => raw,
            Err(err) => {
                self.mark_note_sync_conflict(
                    note_id,
                    "Linked markdown file is missing or unreadable.",
                )?;
                return Err(ServiceError::Other(
                    anyhow::Error::new(err)
                        .context(format!("failed reading {}", target_path.display())),
                ));
            }
        };
        let file_hash = compute_file_hash(&raw);
        if !force && frontmatter.sync_last_seen_hash.as_deref() == Some(file_hash.as_str()) {
            return self
                .db
                .read_entity_snapshot(note_id)?
                .context("synced note not found after sync check")
                .map_err(ServiceError::Other);
        }

        frontmatter.title =
            synced_note_title_from_path(frontmatter.sync_path.as_deref().unwrap_or(""));
        frontmatter.sync_status = Some(NoteSyncStatus::Live);
        frontmatter.sync_last_seen_hash = Some(file_hash);
        frontmatter.sync_last_inbound_at = Some(Utc::now());
        frontmatter.sync_conflict_summary = None;
        frontmatter.sync_conflict_at = None;
        frontmatter.updated_at = Utc::now();

        let action = if force {
            "resolve_note_sync_from_file"
        } else {
            "import_note_sync_from_file"
        };
        let summary = if force {
            "Resolved linked note sync using file"
        } else {
            "Imported linked note from file"
        };

        let snapshot = self.write_note_entity(
            &record.path,
            raw,
            frontmatter,
            NoteWriteContext {
                before_revision: Some(current_revision),
                archived: record.archived,
                actor: Actor::agent("note-sync"),
                action,
                summary,
            },
        )?;
        self.ensure_note_sync_watcher(note_id)?;
        Ok(snapshot)
    }

    fn write_note_sync_file_from_local(
        &self,
        note_id: &str,
        enforce_hash_match: bool,
    ) -> Result<EntitySnapshot, ServiceError> {
        let (record, mut frontmatter, body, current_revision) = self.load_note_state(note_id)?;
        if frontmatter.sync_kind != Some(NoteSyncKind::RepoMarkdown) {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "note is not linked to a repo markdown file"
            )));
        }

        let target_path = self.note_sync_target_path_from_frontmatter(&frontmatter)?;
        let current_hash =
            compute_file_hash_from_path(&target_path).map_err(ServiceError::Other)?;
        if enforce_hash_match {
            if let (Some(known_hash), Some(current_hash)) = (
                frontmatter.sync_last_seen_hash.as_deref(),
                current_hash.as_deref(),
            ) {
                if known_hash != current_hash {
                    self.mark_note_sync_conflict(
                        note_id,
                        "Linked note sync detected external edits before Topside could write back.",
                    )?;
                    return Err(ServiceError::Other(anyhow::anyhow!(
                        "linked markdown file changed externally"
                    )));
                }
            }
        }

        ensure_parent_dir(&target_path).map_err(ServiceError::Other)?;
        atomic_write(&target_path, &body).map_err(ServiceError::Other)?;
        let new_hash = compute_file_hash(&body);
        if let Ok(mut runtime) = self.note_sync_runtime.lock() {
            let state = runtime.notes.entry(note_id.to_string()).or_default();
            state.last_outbound_hash = Some(new_hash.clone());
        }

        frontmatter.title =
            synced_note_title_from_path(frontmatter.sync_path.as_deref().unwrap_or(""));
        frontmatter.sync_status = Some(NoteSyncStatus::Live);
        frontmatter.sync_last_seen_hash = Some(new_hash);
        frontmatter.sync_last_outbound_at = Some(Utc::now());
        frontmatter.sync_conflict_summary = None;
        frontmatter.sync_conflict_at = None;
        frontmatter.updated_at = Utc::now();

        let action = if enforce_hash_match {
            "sync_note_to_repo_file"
        } else {
            "resolve_note_sync_from_local"
        };
        let summary = if enforce_hash_match {
            "Synced linked note to file"
        } else {
            "Resolved linked note sync using Topside"
        };

        let snapshot = self.write_note_entity(
            &record.path,
            body,
            frontmatter,
            NoteWriteContext {
                before_revision: Some(current_revision),
                archived: record.archived,
                actor: Actor::agent("note-sync"),
                action,
                summary,
            },
        )?;
        self.ensure_note_sync_watcher(note_id)?;
        Ok(snapshot)
    }

    fn mark_note_sync_conflict(&self, note_id: &str, summary: &str) -> Result<(), ServiceError> {
        self.clear_note_sync_dirty(note_id);
        let (record, mut frontmatter, body, current_revision) = self.load_note_state(note_id)?;
        if frontmatter.sync_kind != Some(NoteSyncKind::RepoMarkdown) {
            return Ok(());
        }

        frontmatter.sync_status = Some(NoteSyncStatus::Conflict);
        frontmatter.sync_conflict_summary = Some(summary.to_string());
        frontmatter.sync_conflict_at = Some(Utc::now());
        frontmatter.updated_at = Utc::now();

        self.write_note_entity(
            &record.path,
            body,
            frontmatter,
            NoteWriteContext {
                before_revision: Some(current_revision),
                archived: record.archived,
                actor: Actor::agent("note-sync"),
                action: "note_sync_conflict",
                summary: "Linked note sync entered conflict",
            },
        )?;
        Ok(())
    }

    fn restore_managed_task_sync_watchers(&self) -> Result<()> {
        for project in self.list_projects(UNBOUNDED_QUERY_LIMIT, false)? {
            if project.task_sync_enabled
                && project.task_sync_mode == Some(TaskSyncMode::ManagedTodoFile)
                && project.task_sync_status == Some(TaskSyncStatus::Live)
            {
                if let Err(err) = self.ensure_managed_task_sync_watcher(&project) {
                    self.clear_managed_task_sync_watcher(&project.id);
                    let _ = self.pause_managed_task_sync_for_error(&project.id, &err.to_string());
                }
            }
        }
        Ok(())
    }

    fn reconcile_managed_task_sync_project_defaults(&self) -> Result<()> {
        for project in self.list_projects(UNBOUNDED_QUERY_LIMIT, false)? {
            if !project.task_sync_enabled
                || project.task_sync_mode != Some(TaskSyncMode::ManagedTodoFile)
            {
                continue;
            }

            let normalized_sync_file =
                normalize_managed_task_sync_file(project.task_sync_file.as_deref());
            if project.task_sync_file.as_deref() == Some(normalized_sync_file.as_str()) {
                continue;
            }

            self.update_project(
                &project.id,
                ProjectPatch {
                    task_sync_file: Some(Some(normalized_sync_file)),
                    ..ProjectPatch::default()
                },
                &project.revision,
                Actor::agent("task-sync"),
            )
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        }

        Ok(())
    }

    fn ensure_managed_task_sync_watcher(
        &self,
        project: &crate::types::ProjectItem,
    ) -> Result<(), ServiceError> {
        if !project.task_sync_enabled
            || project.task_sync_mode != Some(TaskSyncMode::ManagedTodoFile)
        {
            return Ok(());
        }

        let target_path = self.managed_task_sync_file_path(project)?;
        let parent = target_path
            .parent()
            .map(Path::to_path_buf)
            .with_context(|| "managed task sync file has no parent directory")
            .map_err(ServiceError::Other)?;

        {
            let mut runtime = self.task_sync_runtime.lock().map_err(|_| {
                ServiceError::Other(anyhow::anyhow!("task sync runtime mutex poisoned"))
            })?;
            let state = runtime.projects.entry(project.id.clone()).or_default();
            if state.watcher_path.as_ref() == Some(&target_path) && state.watcher.is_some() {
                return Ok(());
            }
            state.watcher = None;
            state.watcher_path = Some(target_path.clone());
        }

        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .map_err(|err| ServiceError::Other(err.into()))?;
        watcher
            .watch(&parent, RecursiveMode::NonRecursive)
            .map_err(|err| ServiceError::Other(err.into()))?;

        let service = self.clone();
        let project_id = project.id.clone();
        let target_for_thread = target_path.clone();
        let debounce = Duration::from_millis(WATCHER_DEBOUNCE_MS);
        let thread = std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut touched_target = watch_event_hits_target(first, &target_for_thread);

                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(next) => {
                            touched_target =
                                touched_target || watch_event_hits_target(next, &target_for_thread);
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }

                if touched_target {
                    let _ = service.handle_managed_task_sync_file_event(&project_id);
                }
            }
        });

        let mut runtime = self.task_sync_runtime.lock().map_err(|_| {
            ServiceError::Other(anyhow::anyhow!("task sync runtime mutex poisoned"))
        })?;
        let state = runtime.projects.entry(project.id.clone()).or_default();
        if state.watcher_path.as_ref() == Some(&target_path) {
            state.watcher = Some(ManagedTaskSyncWatcherRuntime {
                _watcher: watcher,
                _thread: thread,
            });
        }

        Ok(())
    }

    fn clear_managed_task_sync_dirty(&self, project_id: &str) {
        if let Ok(mut runtime) = self.task_sync_runtime.lock() {
            if let Some(state) = runtime.projects.get_mut(project_id) {
                state.dirty_outbound = false;
                state.last_outbound_hash = None;
            }
        }
    }

    fn clear_managed_task_sync_dirty_flag(&self, project_id: &str) {
        if let Ok(mut runtime) = self.task_sync_runtime.lock() {
            if let Some(state) = runtime.projects.get_mut(project_id) {
                state.dirty_outbound = false;
            }
        }
    }

    fn clear_managed_task_sync_watcher(&self, project_id: &str) {
        if let Ok(mut runtime) = self.task_sync_runtime.lock() {
            if let Some(state) = runtime.projects.get_mut(project_id) {
                state.dirty_outbound = false;
                state.last_outbound_hash = None;
                state.watcher = None;
                state.watcher_path = None;
            }
        }
    }

    fn queue_managed_task_sync_outbound(&self, project_id: &str, _reason: &'static str) {
        let nonce = {
            let Ok(mut runtime) = self.task_sync_runtime.lock() else {
                return;
            };
            let state = runtime.projects.entry(project_id.to_string()).or_default();
            state.dirty_outbound = true;
            state.outbound_nonce = state.outbound_nonce.saturating_add(1);
            state.outbound_nonce
        };

        let service = self.clone();
        let project_id = project_id.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(OUTBOUND_DEBOUNCE_MS));
            let _ = service.flush_managed_task_sync_outbound(&project_id, nonce);
        });
    }

    fn flush_managed_task_sync_outbound(
        &self,
        project_id: &str,
        nonce: u64,
    ) -> Result<(), ServiceError> {
        let should_flush = {
            let runtime = self.task_sync_runtime.lock().map_err(|_| {
                ServiceError::Other(anyhow::anyhow!("task sync runtime mutex poisoned"))
            })?;
            runtime
                .projects
                .get(project_id)
                .map(|state| state.dirty_outbound && state.outbound_nonce == nonce)
                .unwrap_or(false)
        };
        if !should_flush {
            return Ok(());
        }

        let project = match self.get_project_item(project_id) {
            Ok(project) => project,
            Err(err) => {
                self.clear_managed_task_sync_dirty(project_id);
                return Err(err);
            }
        };

        if !project.task_sync_enabled
            || project.task_sync_mode != Some(TaskSyncMode::ManagedTodoFile)
            || project.task_sync_status != Some(TaskSyncStatus::Live)
        {
            self.clear_managed_task_sync_dirty_flag(project_id);
            return Ok(());
        }

        let result = self.write_managed_task_sync_file_from_local(&project, true);
        self.clear_managed_task_sync_dirty_flag(project_id);
        result.map(|_| ())
    }

    fn handle_managed_task_sync_file_event(&self, project_id: &str) -> Result<(), ServiceError> {
        let project = self.get_project_item(project_id)?;
        if !project.task_sync_enabled
            || project.task_sync_mode != Some(TaskSyncMode::ManagedTodoFile)
            || project.task_sync_status != Some(TaskSyncStatus::Live)
        {
            return Ok(());
        }

        let sync_path = self.managed_task_sync_file_path(&project)?;
        let current_hash = compute_file_hash_from_path(&sync_path).map_err(ServiceError::Other)?;
        let (dirty_outbound, last_outbound_hash) = self
            .task_sync_runtime
            .lock()
            .map_err(|_| ServiceError::Other(anyhow::anyhow!("task sync runtime mutex poisoned")))?
            .projects
            .get(project_id)
            .map(|state| (state.dirty_outbound, state.last_outbound_hash.clone()))
            .unwrap_or((false, None));

        if let (Some(current_hash), Some(outbound_hash)) =
            (current_hash.as_deref(), last_outbound_hash.as_deref())
        {
            if current_hash == outbound_hash {
                return Ok(());
            }
        }

        if dirty_outbound {
            self.mark_managed_task_sync_conflict(
                project_id,
                "Managed task sync detected local and file edits before reconciliation.",
            )?;
            return Ok(());
        }

        self.import_managed_task_sync_from_disk(project_id, false)
    }

    fn ensure_tasks_marked_for_managed_sync(
        &self,
        project_id: &str,
        sync_file: &str,
    ) -> Result<(), ServiceError> {
        let tasks = self.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project_id.to_string()),
            assignee: None,
            include_archived: false,
            limit: Some(UNBOUNDED_QUERY_LIMIT),
        })?;

        let mut paths = Vec::new();
        for task in tasks {
            let (record, parsed) = self
                .db
                .parse_entity_from_disk(&task.id)?
                .context("task not found while enabling managed sync")
                .map_err(ServiceError::Other)?;
            let (mut frontmatter, body) = split_task(parsed.frontmatter, parsed.body)?;
            let desired_key =
                ensure_sync_key_for_title(frontmatter.sync_key.as_deref(), &frontmatter.title);
            let needs_update = frontmatter.sync_kind != Some(TaskSyncKind::ManagedTodoFile)
                || frontmatter.sync_path.as_deref() != Some(sync_file)
                || !frontmatter.sync_managed
                || frontmatter.sync_key.as_deref() != Some(desired_key.as_str());
            if !needs_update {
                continue;
            }

            frontmatter.sync_kind = Some(TaskSyncKind::ManagedTodoFile);
            frontmatter.sync_path = Some(sync_file.to_string());
            frontmatter.sync_key = Some(desired_key);
            frontmatter.sync_managed = true;

            let mut entity = EntityFrontmatter::Task(frontmatter);
            let rendered =
                render_entity_markdown(&mut entity, &body).map_err(ServiceError::Other)?;
            atomic_write(&record.path, &rendered).map_err(ServiceError::Other)?;
            paths.push(record.path);
        }

        if !paths.is_empty() {
            self.indexer
                .index_files(&paths)
                .map_err(ServiceError::Other)?;
        }

        Ok(())
    }

    fn managed_task_sync_file_path(
        &self,
        project: &crate::types::ProjectItem,
    ) -> Result<PathBuf, ServiceError> {
        if project.source_kind != Some(crate::types::ProjectSourceKind::Local) {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "managed task sync requires a linked local source folder"
            )));
        }
        let source_locator = project
            .source_locator
            .as_deref()
            .with_context(|| "project has no linked local source folder")
            .map_err(ServiceError::Other)?;
        let source_root = PathBuf::from(source_locator);
        if !source_root.is_dir() {
            return Err(ServiceError::Other(anyhow::anyhow!(
                "linked source path is not a directory"
            )));
        }

        let sync_file = normalize_managed_task_sync_file(project.task_sync_file.as_deref());
        resolve_managed_file_path(source_locator, &sync_file).map_err(ServiceError::Other)
    }

    fn managed_task_sync_sidecar_path(
        &self,
        project: &crate::types::ProjectItem,
    ) -> Result<PathBuf, ServiceError> {
        Ok(managed_todo_sidecar_path(
            &self.managed_task_sync_file_path(project)?,
        ))
    }

    fn legacy_managed_task_sync_sidecar_path(
        &self,
        project: &crate::types::ProjectItem,
    ) -> Result<PathBuf, ServiceError> {
        Ok(legacy_managed_todo_sidecar_path(
            &self.managed_task_sync_file_path(project)?,
        ))
    }

    fn managed_task_sync_entries_from_local(
        &self,
        project_id: &str,
        sync_file: &str,
    ) -> Result<Vec<ManagedTodoRenderEntry>, ServiceError> {
        let mut tasks = self.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project_id.to_string()),
            assignee: None,
            include_archived: false,
            limit: Some(UNBOUNDED_QUERY_LIMIT),
        })?;
        tasks.retain(|task| {
            task.sync_managed
                && task.sync_kind == Some(TaskSyncKind::ManagedTodoFile)
                && task.sync_path.as_deref() == Some(sync_file)
        });
        tasks.sort_by(|left, right| {
            effective_task_sort_order(left)
                .cmp(&effective_task_sort_order(right))
                .then(left.created_at.cmp(&right.created_at))
        });

        Ok(tasks
            .iter()
            .filter_map(render_entry_from_task)
            .collect::<Vec<_>>())
    }

    fn managed_task_sync_sidecar_entries(
        &self,
        project: &crate::types::ProjectItem,
        sync_file: &str,
    ) -> Result<(Vec<ManagedTodoRenderEntry>, bool), ServiceError> {
        let local_entries = self.managed_task_sync_entries_from_local(&project.id, sync_file)?;
        let sidecar_path = self.managed_task_sync_sidecar_path(project)?;
        let legacy_sidecar_path = self.legacy_managed_task_sync_sidecar_path(project)?;
        let existing_sidecar_path = if sidecar_path.exists() {
            sidecar_path
        } else if legacy_sidecar_path.exists() {
            legacy_sidecar_path
        } else {
            return Ok((local_entries, false));
        };
        if !existing_sidecar_path.exists() {
            return Ok((local_entries, false));
        }

        let raw = match std::fs::read_to_string(&existing_sidecar_path) {
            Ok(raw) => raw,
            Err(_) => return Ok((local_entries, true)),
        };
        let sidecar = match parse_managed_todo_sidecar(&raw) {
            Ok(sidecar) => sidecar,
            Err(_) => return Ok((local_entries, true)),
        };
        if !local_entries.is_empty()
            && (sidecar.entries.is_empty() || sidecar.entries != local_entries)
        {
            return Ok((local_entries, true));
        }
        Ok((sidecar.entries, false))
    }

    fn write_managed_task_sync_sidecar(
        &self,
        sidecar_path: &Path,
        entries: &[ManagedTodoRenderEntry],
    ) -> Result<(), ServiceError> {
        let sidecar = render_managed_todo_sidecar(entries).map_err(ServiceError::Other)?;
        ensure_parent_dir(sidecar_path).map_err(ServiceError::Other)?;
        atomic_write(sidecar_path, &sidecar).map_err(ServiceError::Other)
    }

    fn remove_legacy_managed_task_sync_sidecar(
        &self,
        project: &crate::types::ProjectItem,
    ) -> Result<(), ServiceError> {
        let legacy_sidecar_path = self.legacy_managed_task_sync_sidecar_path(project)?;
        if legacy_sidecar_path.exists() {
            std::fs::remove_file(&legacy_sidecar_path).with_context(|| {
                format!(
                    "failed removing legacy managed task sync sidecar {}",
                    legacy_sidecar_path.display()
                )
            })?;
        }
        Ok(())
    }

    fn write_managed_task_sync_sidecar_from_local(
        &self,
        project: &crate::types::ProjectItem,
    ) -> Result<(), ServiceError> {
        let sync_file = normalize_managed_task_sync_file(project.task_sync_file.as_deref());
        let entries = self.managed_task_sync_entries_from_local(&project.id, &sync_file)?;
        let sidecar_path = self.managed_task_sync_sidecar_path(project)?;
        self.write_managed_task_sync_sidecar(&sidecar_path, &entries)?;
        self.remove_legacy_managed_task_sync_sidecar(project)
    }

    fn write_managed_task_sync_file_from_local(
        &self,
        project: &crate::types::ProjectItem,
        enforce_hash_match: bool,
    ) -> Result<String, ServiceError> {
        let sync_path = self.managed_task_sync_file_path(project)?;
        let current_hash = compute_file_hash_from_path(&sync_path).map_err(ServiceError::Other)?;
        if enforce_hash_match {
            if let (Some(known_hash), Some(current_hash)) = (
                project.task_sync_last_seen_hash.as_deref(),
                current_hash.as_deref(),
            ) {
                if known_hash != current_hash {
                    self.mark_managed_task_sync_conflict(
                        &project.id,
                        "Managed task sync detected external edits before Topside could write back.",
                    )?;
                    return Err(ServiceError::Other(anyhow::anyhow!(
                        "managed task sync file changed externally"
                    )));
                }
            }
        }

        let sync_file = normalize_managed_task_sync_file(project.task_sync_file.as_deref());
        let entries = self.managed_task_sync_entries_from_local(&project.id, &sync_file)?;
        let content = render_managed_todo(&entries);
        ensure_parent_dir(&sync_path).map_err(ServiceError::Other)?;
        atomic_write(&sync_path, &content).map_err(ServiceError::Other)?;
        let sidecar_path = self.managed_task_sync_sidecar_path(project)?;
        self.write_managed_task_sync_sidecar(&sidecar_path, &entries)?;
        self.remove_legacy_managed_task_sync_sidecar(project)?;
        let new_hash = compute_file_hash(&content);
        if let Ok(mut runtime) = self.task_sync_runtime.lock() {
            let state = runtime.projects.entry(project.id.clone()).or_default();
            state.last_outbound_hash = Some(new_hash.clone());
        }

        let refreshed_project = self.get_project_item(&project.id)?;
        self.update_project(
            &project.id,
            ProjectPatch {
                task_sync_last_seen_hash: Some(Some(new_hash.clone())),
                task_sync_last_outbound_at: Some(Some(Utc::now())),
                task_sync_status: Some(Some(TaskSyncStatus::Live)),
                task_sync_conflict_summary: Some(None),
                task_sync_conflict_at: Some(None),
                ..ProjectPatch::default()
            },
            &refreshed_project.revision,
            Actor::agent("task-sync"),
        )?;

        Ok(new_hash)
    }

    fn import_managed_task_sync_from_disk(
        &self,
        project_id: &str,
        force: bool,
    ) -> Result<(), ServiceError> {
        let project = self.get_project_item(project_id)?;
        let sync_path = match self.managed_task_sync_file_path(&project) {
            Ok(path) => path,
            Err(err) => {
                self.pause_managed_task_sync_for_error(project_id, &err.to_string())?;
                return Err(err);
            }
        };

        if !sync_path.exists() {
            if !force && project.task_sync_status == Some(TaskSyncStatus::Conflict) {
                return Ok(());
            }
            self.write_managed_task_sync_file_from_local(&project, false)?;
            return Ok(());
        }

        let raw = std::fs::read_to_string(&sync_path)
            .with_context(|| format!("failed reading {}", sync_path.display()))
            .map_err(ServiceError::Other)?;
        let file_hash = compute_file_hash(&raw);
        let sync_file = normalize_managed_task_sync_file(project.task_sync_file.as_deref());
        let (sidecar_entries, sidecar_needs_rewrite) =
            self.managed_task_sync_sidecar_entries(&project, &sync_file)?;
        let sidecar_path = self.managed_task_sync_sidecar_path(&project)?;
        if !force
            && sidecar_path.exists()
            && !sidecar_needs_rewrite
            && project.task_sync_last_seen_hash.as_deref() == Some(file_hash.as_str())
        {
            return Ok(());
        }

        let parsed = parse_managed_todo(&raw, &sidecar_entries);
        let needs_rewrite =
            self.reconcile_managed_task_sync_entries(project_id, &sync_file, parsed.entries)?;

        if needs_rewrite || parsed.had_inline_sync_keys {
            let refreshed_project = self.get_project_item(project_id)?;
            self.write_managed_task_sync_file_from_local(&refreshed_project, false)?;
            let latest_project = self.get_project_item(project_id)?;
            self.update_project(
                project_id,
                ProjectPatch {
                    task_sync_last_inbound_at: Some(Some(Utc::now())),
                    task_sync_status: Some(Some(TaskSyncStatus::Live)),
                    task_sync_conflict_summary: Some(None),
                    task_sync_conflict_at: Some(None),
                    ..ProjectPatch::default()
                },
                &latest_project.revision,
                Actor::agent("task-sync"),
            )?;
            return Ok(());
        }

        let refreshed_project = self.get_project_item(project_id)?;
        self.write_managed_task_sync_sidecar_from_local(&refreshed_project)?;

        let latest_project = self.get_project_item(project_id)?;
        self.update_project(
            project_id,
            ProjectPatch {
                task_sync_last_seen_hash: Some(Some(file_hash)),
                task_sync_last_inbound_at: Some(Some(Utc::now())),
                task_sync_status: Some(Some(TaskSyncStatus::Live)),
                task_sync_conflict_summary: Some(None),
                task_sync_conflict_at: Some(None),
                ..ProjectPatch::default()
            },
            &latest_project.revision,
            Actor::agent("task-sync"),
        )?;
        Ok(())
    }

    fn reconcile_managed_task_sync_entries(
        &self,
        project_id: &str,
        sync_file: &str,
        entries: Vec<ParsedManagedTodoEntry>,
    ) -> Result<bool, ServiceError> {
        let existing_tasks = self.list_tasks(&TaskFilters {
            status: None,
            priority: None,
            project_id: Some(project_id.to_string()),
            assignee: None,
            include_archived: false,
            limit: Some(UNBOUNDED_QUERY_LIMIT),
        })?;

        let mut existing_by_key = HashMap::new();
        for task in existing_tasks.iter().cloned() {
            if task.sync_managed
                && task.sync_kind == Some(TaskSyncKind::ManagedTodoFile)
                && task.sync_path.as_deref() == Some(sync_file)
            {
                if let Some(sync_key) = task.sync_key.clone() {
                    existing_by_key.insert(sync_key, task);
                }
            }
        }

        let mut seen_sync_keys = HashSet::new();
        let mut pending_updates = Vec::new();
        let mut pending_creates = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let desired_title = task_title_from_entry(entry);
            let desired_status = match entry.kind {
                ManagedTodoEntryKind::Task { completed: true } => TaskStatus::Done,
                _ => TaskStatus::Todo,
            };
            let desired_sort_order = (index as i64) + 1;
            seen_sync_keys.insert(entry.sync_key.clone());

            if let Some(existing) = existing_by_key.get(&entry.sync_key) {
                let mut patch = TaskPatch::default();
                let mut changed = false;

                if existing.title != desired_title {
                    patch.title = Some(desired_title.clone());
                    changed = true;
                }
                if existing.status != desired_status {
                    patch.status = Some(desired_status);
                    changed = true;
                }
                if existing.sort_order != desired_sort_order {
                    patch.sort_order = Some(desired_sort_order);
                    changed = true;
                }

                if changed {
                    pending_updates.push(TaskUpdateRequest {
                        id: existing.id.clone(),
                        expected_revision: existing.revision.clone(),
                        patch,
                    });
                }
                continue;
            }

            pending_creates.push(CreateTaskPayload {
                title: desired_title,
                project_id: project_id.to_string(),
                status: Some(desired_status),
                priority: Some(TaskPriority::P0),
                assignee: Some("agent:unassigned".to_string()),
                due_at: None,
                sort_order: Some(desired_sort_order),
                sync_kind: Some(TaskSyncKind::ManagedTodoFile),
                sync_path: Some(sync_file.to_string()),
                sync_key: Some(entry.sync_key.clone()),
                sync_managed: Some(true),
                tags: None,
                body: Some(String::new()),
            });
        }

        if !pending_updates.is_empty() {
            self.update_tasks(pending_updates, Actor::agent("task-sync"))?;
        }
        if !pending_creates.is_empty() {
            self.create_tasks(pending_creates, Actor::agent("task-sync"))?;
        }

        let mut stale = Vec::new();
        for task in existing_tasks {
            if task.sync_managed
                && task.sync_kind == Some(TaskSyncKind::ManagedTodoFile)
                && task.sync_path.as_deref() == Some(sync_file)
            {
                if let Some(sync_key) = task.sync_key.as_deref() {
                    if !seen_sync_keys.contains(sync_key) {
                        stale.push(ArchiveEntityRequest {
                            id: task.id.clone(),
                            expected_revision: task.revision.clone(),
                        });
                    }
                }
            }
        }
        if !stale.is_empty() {
            self.archive_entities(stale, Actor::agent("task-sync"))?;
        }

        Ok(false)
    }

    fn mark_managed_task_sync_conflict(
        &self,
        project_id: &str,
        summary: &str,
    ) -> Result<(), ServiceError> {
        self.clear_managed_task_sync_dirty(project_id);
        let project = self.get_project_item(project_id)?;
        self.update_project(
            project_id,
            ProjectPatch {
                task_sync_status: Some(Some(TaskSyncStatus::Conflict)),
                task_sync_conflict_summary: Some(Some(summary.to_string())),
                task_sync_conflict_at: Some(Some(Utc::now())),
                ..ProjectPatch::default()
            },
            &project.revision,
            Actor::agent("task-sync"),
        )?;
        self.record_project_sync_activity(
            Actor::agent("task-sync"),
            project_id,
            "task_sync_conflict",
            summary,
        )?;
        Ok(())
    }

    fn pause_managed_task_sync_for_error(
        &self,
        project_id: &str,
        summary: &str,
    ) -> Result<(), ServiceError> {
        self.clear_managed_task_sync_dirty(project_id);
        let project = self.get_project_item(project_id)?;
        self.update_project(
            project_id,
            ProjectPatch {
                task_sync_status: Some(Some(TaskSyncStatus::Paused)),
                task_sync_conflict_summary: Some(Some(summary.to_string())),
                task_sync_conflict_at: Some(Some(Utc::now())),
                ..ProjectPatch::default()
            },
            &project.revision,
            Actor::agent("task-sync"),
        )?;
        self.record_project_sync_activity(
            Actor::agent("task-sync"),
            project_id,
            "task_sync_paused",
            format!("Paused managed task sync: {summary}"),
        )?;
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

    fn enforce_current_revision(&self, expected: &str, current: &str) -> Result<(), ServiceError> {
        if current == expected {
            return Ok(());
        }

        Err(ServiceError::Conflict {
            expected: expected.to_string(),
            current: current.to_string(),
        })
    }

    fn record_entity_activity(&self, actor: Actor, meta: EntityActivityMeta<'_>) -> Result<()> {
        let git = self.cached_git_context();
        let draft = ActivityDraft::new(actor, meta.action, meta.summary)
            .with_entity(meta.entity_type, meta.entity_id.to_string())
            .with_path(meta.path.to_string_lossy().to_string())
            .with_revisions(meta.before_revision, meta.after_revision)
            .with_git(git.branch, git.commit);
        self.db.record_activity(draft)?;
        Ok(())
    }

    fn record_entity_activities(
        &self,
        actor: &Actor,
        metas: Vec<OwnedEntityActivityMeta>,
    ) -> Result<(), ServiceError> {
        if metas.is_empty() {
            return Ok(());
        }

        let git = self.cached_git_context();
        let drafts = metas
            .into_iter()
            .map(|meta| {
                ActivityDraft::new(actor.clone(), meta.action, meta.summary)
                    .with_entity(meta.entity_type, meta.entity_id)
                    .with_path(meta.path.to_string_lossy().to_string())
                    .with_revisions(meta.before_revision, meta.after_revision)
                    .with_git(git.branch.clone(), git.commit.clone())
            })
            .collect::<Vec<_>>();
        self.db.record_activities(drafts)?;
        Ok(())
    }

    fn record_project_sync_activity(
        &self,
        actor: Actor,
        project_id: &str,
        action: &'static str,
        summary: impl Into<String>,
    ) -> Result<(), ServiceError> {
        let project = self.get_project_item(project_id)?;
        let project_path = PathBuf::from(&project.path);
        let summary = summary.into();
        self.record_entity_activity(
            actor,
            EntityActivityMeta {
                action,
                entity_type: EntityType::Project,
                entity_id: &project.id,
                path: &project_path,
                before_revision: None,
                after_revision: None,
                summary: &summary,
            },
        )
        .map_err(ServiceError::Other)
    }

    fn cached_git_context(&self) -> GitContext {
        let now = Instant::now();
        let mut cache = match self.git_context_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(entry) = cache.as_ref() {
            if now.duration_since(entry.captured_at) <= GIT_CONTEXT_CACHE_TTL {
                return entry.context.clone();
            }
        }

        let context = read_git_context(&self.config.workspace_root);
        *cache = Some(GitContextCacheEntry {
            captured_at: now,
            context: context.clone(),
        });
        context
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

struct OwnedEntityActivityMeta {
    action: &'static str,
    entity_type: EntityType,
    entity_id: String,
    path: PathBuf,
    before_revision: Option<String>,
    after_revision: Option<String>,
    summary: &'static str,
}

struct PendingMutation {
    path: PathBuf,
    body: String,
    frontmatter: EntityFrontmatter,
    before_revision: Option<String>,
    archived: bool,
}

const GIT_CONTEXT_CACHE_TTL: Duration = Duration::from_secs(2);

struct GitContextCacheEntry {
    captured_at: Instant,
    context: GitContext,
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

#[derive(Default)]
struct ManagedTaskSyncRuntime {
    projects: HashMap<String, ManagedTaskSyncProjectRuntime>,
}

#[derive(Default)]
struct ManagedNoteSyncRuntime {
    notes: HashMap<String, ManagedNoteSyncNoteRuntime>,
}

#[derive(Default)]
struct ManagedTaskSyncProjectRuntime {
    dirty_outbound: bool,
    outbound_nonce: u64,
    last_outbound_hash: Option<String>,
    watcher_path: Option<PathBuf>,
    watcher: Option<ManagedTaskSyncWatcherRuntime>,
}

#[derive(Default)]
struct ManagedNoteSyncNoteRuntime {
    dirty_outbound: bool,
    outbound_nonce: u64,
    last_outbound_hash: Option<String>,
    watcher_path: Option<PathBuf>,
    watcher: Option<ManagedNoteSyncWatcherRuntime>,
}

struct ManagedTaskSyncWatcherRuntime {
    _watcher: RecommendedWatcher,
    _thread: std::thread::JoinHandle<()>,
}

struct ManagedNoteSyncWatcherRuntime {
    _watcher: RecommendedWatcher,
    _thread: std::thread::JoinHandle<()>,
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

fn watch_event_hits_target(event: notify::Result<Event>, target: &Path) -> bool {
    match event {
        Ok(event) => {
            let is_remove = matches!(event.kind, EventKind::Remove(_));
            event.paths.into_iter().any(|path| {
                if path == target {
                    return true;
                }
                is_remove && path == target
            })
        }
        Err(_) => false,
    }
}

fn synced_note_title_from_path(sync_path: &str) -> String {
    Path::new(sync_path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| "Linked doc".to_string())
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

fn snapshot_from_parts(
    path: &Path,
    body: String,
    frontmatter: EntityFrontmatter,
    revision: String,
    archived: bool,
) -> EntitySnapshot {
    EntitySnapshot {
        id: frontmatter.id().to_string(),
        entity_type: frontmatter.entity_type(),
        title: frontmatter.title().to_string(),
        path: path.to_string_lossy().to_string(),
        body,
        frontmatter,
        revision,
        archived,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn build_codex_execute_prompt_is_task_only() -> Result<()> {
        let workspace = TempDir::new()?;
        let repo_root = TempDir::new()?;
        let config = AppConfig::default_for_workspace(workspace.path().to_path_buf());
        config.ensure_workspace_dirs()?;
        let service = AppService::bootstrap(config)?;

        let project = service.create_project(
            CreateProjectPayload {
                title: "Codex Assignment Project".to_string(),
                owner: None,
                source_kind: Some(crate::types::ProjectSourceKind::Local),
                source_locator: Some(repo_root.path().to_string_lossy().to_string()),
                icon: None,
                tags: None,
                body: None,
            },
            Actor::human("tester"),
        )?;
        let task = service.create_task(
            CreateTaskPayload {
                title: "Fix flaky auth redirect".to_string(),
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
                body: Some(String::new()),
            },
            Actor::human("tester"),
        )?;

        let prompt = service.build_codex_execute_prompt(&project.id, &task.id, &task.title)?;

        assert_eq!(
            prompt,
            "Execute the following task: Fix flaky auth redirect"
        );

        Ok(())
    }
}
