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
use crate::types::{
    Actor, CreateNotePayload, CreateProjectPayload, CreateTaskPayload, EntityFrontmatter,
    EntitySnapshot, EntityType, NoteFrontmatter, NoteItem, NotePatch, ParsedEntity,
    ProjectFrontmatter, ProjectStatus, SearchFilters, SearchResult, TaskFilters, TaskFrontmatter,
    TaskItem, TaskPatch, TaskPriority, TaskStatus,
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

        let mut fm = EntityFrontmatter::Project(ProjectFrontmatter {
            id: id.clone(),
            entity_type: EntityType::Project,
            title: payload.title,
            status: ProjectStatus::Active,
            owner: payload.owner,
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

        let now = Utc::now();
        let id = format!("{}_{}", EntityType::Task.prefix(), Ulid::new());
        let title_slug = slugify(&payload.title);
        let path = self
            .config
            .tasks_dir()
            .join(&payload.project_id)
            .join(format!("{id}-{title_slug}.md"));
        self.config.ensure_within_workspace(&path)?;

        let mut fm = EntityFrontmatter::Task(TaskFrontmatter {
            id: id.clone(),
            entity_type: EntityType::Task,
            title: payload.title,
            project_id: payload.project_id,
            status: payload.status.unwrap_or_default(),
            priority: payload.priority.unwrap_or_default(),
            assignee: payload
                .assignee
                .unwrap_or_else(|| "agent:unassigned".to_string()),
            due_at: payload.due_at,
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

fn slugify(value: &str) -> String {
    let mut slug = value
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();

    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }

    slug.trim_matches('-').chars().take(64).collect()
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
