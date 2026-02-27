use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Task,
    Project,
    Note,
}

impl EntityType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Project => "project",
            Self::Note => "note",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "task" => Some(Self::Task),
            "project" => Some(Self::Project),
            "note" => Some(Self::Note),
            _ => None,
        }
    }

    pub fn prefix(self) -> &'static str {
        match self {
            Self::Task => "tsk",
            Self::Project => "prj",
            Self::Note => "nte",
        }
    }
}

impl fmt::Display for EntityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Backlog,
    Todo,
    InProgress,
    Blocked,
    Done,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

impl Default for TaskStatus {
    fn default() -> Self {
        Self::Todo
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskPriority {
    #[serde(rename = "P0")]
    P0,
    #[serde(rename = "P1")]
    P1,
    #[serde(rename = "P2")]
    P2,
    #[serde(rename = "P3")]
    P3,
}

impl TaskPriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
            Self::P3 => "P3",
        }
    }
}

impl Default for TaskPriority {
    fn default() -> Self {
        Self::P2
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Active,
    Paused,
    Archived,
}

impl ProjectStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Archived => "archived",
        }
    }
}

impl Default for ProjectStatus {
    fn default() -> Self {
        Self::Active
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSourceKind {
    Local,
    Github,
}

impl ProjectSourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Github => "github",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskFrontmatter {
    pub id: String,
    #[serde(rename = "type")]
    pub entity_type: EntityType,
    pub title: String,
    pub project_id: String,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub assignee: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub sort_order: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFrontmatter {
    pub id: String,
    #[serde(rename = "type")]
    pub entity_type: EntityType,
    pub title: String,
    pub status: ProjectStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<ProjectSourceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_locator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteFrontmatter {
    pub id: String,
    #[serde(rename = "type")]
    pub entity_type: EntityType,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EntityFrontmatter {
    Task(TaskFrontmatter),
    Project(ProjectFrontmatter),
    Note(NoteFrontmatter),
}

impl EntityFrontmatter {
    pub fn id(&self) -> &str {
        match self {
            Self::Task(v) => &v.id,
            Self::Project(v) => &v.id,
            Self::Note(v) => &v.id,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Self::Task(v) => &v.title,
            Self::Project(v) => &v.title,
            Self::Note(v) => &v.title,
        }
    }

    pub fn revision(&self) -> &str {
        match self {
            Self::Task(v) => &v.revision,
            Self::Project(v) => &v.revision,
            Self::Note(v) => &v.revision,
        }
    }

    pub fn set_revision(&mut self, revision: String) {
        match self {
            Self::Task(v) => v.revision = revision,
            Self::Project(v) => v.revision = revision,
            Self::Note(v) => v.revision = revision,
        }
    }

    pub fn set_updated_now(&mut self, now: DateTime<Utc>) {
        match self {
            Self::Task(v) => v.updated_at = now,
            Self::Project(v) => v.updated_at = now,
            Self::Note(v) => v.updated_at = now,
        }
    }

    pub fn entity_type(&self) -> EntityType {
        match self {
            Self::Task(_) => EntityType::Task,
            Self::Project(_) => EntityType::Project,
            Self::Note(_) => EntityType::Note,
        }
    }

    pub fn tags(&self) -> Option<&Vec<String>> {
        match self {
            Self::Task(v) => v.tags.as_ref(),
            Self::Project(v) => v.tags.as_ref(),
            Self::Note(v) => v.tags.as_ref(),
        }
    }

    pub fn project_id(&self) -> Option<&str> {
        match self {
            Self::Task(v) => Some(&v.project_id),
            Self::Note(v) => v.project_id.as_deref(),
            Self::Project(_) => None,
        }
    }

    pub fn status(&self) -> Option<String> {
        match self {
            Self::Task(v) => Some(v.status.as_str().to_string()),
            Self::Project(v) => Some(v.status.as_str().to_string()),
            Self::Note(_) => None,
        }
    }

    pub fn priority(&self) -> Option<String> {
        match self {
            Self::Task(v) => Some(v.priority.as_str().to_string()),
            _ => None,
        }
    }

    pub fn assignee(&self) -> Option<&str> {
        match self {
            Self::Task(v) => Some(&v.assignee),
            _ => None,
        }
    }

    pub fn due_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Task(v) => v.due_at,
            _ => None,
        }
    }

    pub fn sort_order(&self) -> Option<i64> {
        match self {
            Self::Task(v) => Some(v.sort_order),
            _ => None,
        }
    }

    pub fn completed_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Task(v) => v.completed_at,
            _ => None,
        }
    }

    pub fn owner(&self) -> Option<&str> {
        match self {
            Self::Project(v) => v.owner.as_deref(),
            _ => None,
        }
    }

    pub fn source_kind(&self) -> Option<ProjectSourceKind> {
        match self {
            Self::Project(v) => v.source_kind.clone(),
            _ => None,
        }
    }

    pub fn source_locator(&self) -> Option<&str> {
        match self {
            Self::Project(v) => v.source_locator.as_deref(),
            _ => None,
        }
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        match self {
            Self::Task(v) => v.created_at,
            Self::Project(v) => v.created_at,
            Self::Note(v) => v.created_at,
        }
    }

    pub fn updated_at(&self) -> DateTime<Utc> {
        match self {
            Self::Task(v) => v.updated_at,
            Self::Project(v) => v.updated_at,
            Self::Note(v) => v.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiLink {
    pub target_type: EntityType,
    pub target_id: String,
    pub raw: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedEntity {
    pub frontmatter: EntityFrontmatter,
    pub body: String,
    pub revision: String,
    pub links: Vec<WikiLink>,
}

#[derive(Debug, Clone)]
pub struct IndexedEntity {
    pub id: String,
    pub entity_type: EntityType,
    pub title: String,
    pub path: PathBuf,
    pub body: String,
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub assignee: Option<String>,
    pub due_at: Option<DateTime<Utc>>,
    pub sort_order: i64,
    pub completed_at: Option<DateTime<Utc>>,
    pub owner: Option<String>,
    pub source_kind: Option<ProjectSourceKind>,
    pub source_locator: Option<String>,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub revision: String,
    pub archived: bool,
    pub links: Vec<WikiLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskFilters {
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub priority: Option<TaskPriority>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchFilters {
    #[serde(default)]
    pub entity_type: Option<EntityType>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub entity_type: EntityType,
    pub title: String,
    pub path: String,
    pub score: f64,
    pub snippet: String,
    pub revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySnapshot {
    pub id: String,
    pub entity_type: EntityType,
    pub title: String,
    pub path: String,
    pub body: String,
    pub frontmatter: EntityFrontmatter,
    pub revision: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskItem {
    pub id: String,
    pub title: String,
    pub project_id: String,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub assignee: String,
    pub due_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub sort_order: i64,
    pub completed_at: Option<DateTime<Utc>>,
    pub path: String,
    pub updated_at: DateTime<Utc>,
    pub revision: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteItem {
    pub id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub path: String,
    pub updated_at: DateTime<Utc>,
    pub revision: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteDetail {
    pub id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub body: String,
    pub path: String,
    pub updated_at: DateTime<Utc>,
    pub revision: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectItem {
    pub id: String,
    pub title: String,
    pub status: String,
    pub owner: Option<String>,
    pub source_kind: Option<ProjectSourceKind>,
    pub source_locator: Option<String>,
    pub path: String,
    pub updated_at: DateTime<Utc>,
    pub revision: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityItem {
    pub event_id: String,
    pub occurred_at: DateTime<Utc>,
    pub request_id: String,
    pub actor_kind: String,
    pub actor_id: String,
    pub action: String,
    pub entity_type: Option<EntityType>,
    pub entity_id: Option<String>,
    pub file_path: Option<String>,
    pub before_revision: Option<String>,
    pub after_revision: Option<String>,
    pub summary: String,
    pub git_branch: Option<String>,
    pub git_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectWorkspace {
    pub project: ProjectItem,
    pub active_tasks: Vec<TaskItem>,
    pub done_tasks: Vec<TaskItem>,
    pub notes: Vec<NoteDetail>,
    pub suggested_open_note_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProjectPayload {
    pub title: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub source_kind: Option<ProjectSourceKind>,
    #[serde(default)]
    pub source_locator: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskPayload {
    pub title: String,
    pub project_id: String,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub priority: Option<TaskPriority>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub due_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub sort_order: Option<i64>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateNotePayload {
    pub title: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub priority: Option<TaskPriority>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub due_at: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i64>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskReorderPayload {
    pub project_id: String,
    pub ordered_active_task_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotePatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectPatch {
    pub title: Option<String>,
    pub status: Option<ProjectStatus>,
    pub owner: Option<Option<String>>,
    pub source_kind: Option<Option<ProjectSourceKind>>,
    pub source_locator: Option<Option<String>>,
    pub tags: Option<Vec<String>>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub kind: String,
    pub id: String,
}

impl Actor {
    pub fn human(id: impl Into<String>) -> Self {
        Self {
            kind: "human".to_string(),
            id: id.into(),
        }
    }

    pub fn agent(id: impl Into<String>) -> Self {
        Self {
            kind: "agent".to_string(),
            id: id.into(),
        }
    }
}
