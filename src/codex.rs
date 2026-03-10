use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{info, warn};
use ulid::Ulid;
use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::markdown::split_frontmatter;
use crate::service::{AppService, ServiceError};
use crate::task_sync::is_heading_title;
use crate::types::{Actor, EntityFrontmatter, TaskPatch, TaskStatus};

const CODEX_SESSION_TYPE: &str = "codex_session";
const OUTPUT_BACKLOG_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_PTY_ROWS: u16 = 30;
const DEFAULT_PTY_COLS: u16 = 110;
const CODEX_RECONCILE_TIMEOUT: Duration = Duration::from_secs(12);
const CODEX_RECONCILE_POLL_INTERVAL: Duration = Duration::from_millis(600);
const CODEX_HISTORY_MATCH_GRACE_SECONDS: i64 = 2;
const TASK_ASSIGNEE_CODEX: &str = "agent:codex";
const TASK_ASSIGNEE_UNASSIGNED: &str = "agent:unassigned";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodexSessionOrigin {
    Topside,
    Discovered,
}

impl CodexSessionOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Topside => "topside",
            Self::Discovered => "discovered",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodexSessionStatus {
    Launching,
    Live,
    Resumable,
}

impl CodexSessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Live => "live",
            Self::Resumable => "resumable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexSessionFrontmatter {
    pub id: String,
    #[serde(rename = "type")]
    pub session_type: String,
    pub project_id: String,
    #[serde(default)]
    pub task_id: Option<String>,
    pub title: String,
    pub origin: CodexSessionOrigin,
    pub status: CodexSessionStatus,
    pub cwd: String,
    #[serde(default)]
    pub codex_session_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexSessionRecord {
    pub id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub title: String,
    pub origin: CodexSessionOrigin,
    pub status: CodexSessionStatus,
    pub cwd: String,
    pub codex_session_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub summary: String,
    pub path: String,
}

impl CodexSessionRecord {
    pub fn is_live(&self) -> bool {
        self.status == CodexSessionStatus::Live
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodexSessionPatch {
    pub title: Option<String>,
    pub task_id: Option<Option<String>>,
    pub status: Option<CodexSessionStatus>,
    pub cwd: Option<String>,
    pub codex_session_id: Option<Option<String>>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub ended_at: Option<Option<DateTime<Utc>>>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexSessionCounts {
    pub total_count: usize,
    pub live_count: usize,
}

#[derive(Debug, Clone)]
pub struct NewCodexSession {
    pub project_id: String,
    pub task_id: Option<String>,
    pub title: String,
    pub origin: CodexSessionOrigin,
    pub status: CodexSessionStatus,
    pub cwd: String,
    pub codex_session_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct CodexHistorySession {
    pub codex_session_id: String,
    pub thread_name: String,
    pub cwd: PathBuf,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodexTranscriptRole {
    User,
    Assistant,
}

impl CodexTranscriptRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodexTranscriptMessage {
    pub role: CodexTranscriptRole,
    pub text: String,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct AssignableTaskState {
    project_id: String,
    title: String,
    status: TaskStatus,
    assignee: String,
    revision: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodexTerminalServerMessage {
    Output { data: String },
    Status { status: String },
    Error { message: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodexTerminalClientMessage {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

#[derive(Clone)]
pub struct CodexSessionStore {
    config: AppConfig,
}

impl CodexSessionStore {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    pub fn list_project_sessions(&self, project_id: &str) -> Result<Vec<CodexSessionRecord>> {
        let mut sessions = keep_topside_codex_sessions(dedupe_codex_sessions(
            self.list_project_sessions_raw(project_id)?,
        ));
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    pub fn list_all_sessions(&self) -> Result<Vec<CodexSessionRecord>> {
        let mut sessions =
            keep_topside_codex_sessions(dedupe_codex_sessions(self.list_all_sessions_raw()?));
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn list_project_sessions_raw(&self, project_id: &str) -> Result<Vec<CodexSessionRecord>> {
        let dir = self.project_dir(project_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in
            fs::read_dir(&dir).with_context(|| format!("failed reading {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            sessions.push(self.read_record(&path)?);
        }
        Ok(sessions)
    }

    fn list_all_sessions_raw(&self) -> Result<Vec<CodexSessionRecord>> {
        let agents_dir = self.config.agents_dir();
        if !agents_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in WalkDir::new(&agents_dir) {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            sessions.push(self.read_record(path)?);
        }
        Ok(sessions)
    }

    pub fn list_counts_by_project(&self) -> Result<HashMap<String, CodexSessionCounts>> {
        let mut counts = HashMap::<String, CodexSessionCounts>::new();
        for session in self.list_all_sessions()? {
            let entry = counts.entry(session.project_id.clone()).or_default();
            entry.total_count += 1;
            if session.status == CodexSessionStatus::Live {
                entry.live_count += 1;
            }
        }
        Ok(counts)
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<CodexSessionRecord>> {
        for session in self.list_all_sessions_raw()? {
            if session.id == session_id {
                return Ok(Some(session));
            }
        }
        Ok(None)
    }

    pub fn find_by_codex_session_id(
        &self,
        codex_session_id: &str,
    ) -> Result<Option<CodexSessionRecord>> {
        for session in self.list_all_sessions()? {
            if session.codex_session_id.as_deref() == Some(codex_session_id) {
                return Ok(Some(session));
            }
        }
        Ok(None)
    }

    pub fn create_session(&self, new_session: NewCodexSession) -> Result<CodexSessionRecord> {
        let id = format!("ags_{}", Ulid::new());
        let path = self.session_path(&new_session.project_id, &id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed creating {}", parent.display()))?;
        }

        let frontmatter = CodexSessionFrontmatter {
            id,
            session_type: CODEX_SESSION_TYPE.to_string(),
            project_id: new_session.project_id,
            task_id: new_session.task_id,
            title: new_session.title,
            origin: new_session.origin,
            status: new_session.status,
            cwd: new_session.cwd,
            codex_session_id: new_session.codex_session_id,
            started_at: new_session.started_at,
            last_seen_at: new_session.last_seen_at,
            ended_at: new_session.ended_at,
        };
        self.write_record(&path, &frontmatter, &new_session.summary)
    }

    pub fn update_session(
        &self,
        session_id: &str,
        patch: CodexSessionPatch,
    ) -> Result<CodexSessionRecord> {
        let current = self
            .get_session(session_id)?
            .with_context(|| format!("codex session {session_id} not found"))?;
        let path = PathBuf::from(&current.path);
        let mut frontmatter = current.to_frontmatter();
        let mut summary = current.summary;

        if let Some(value) = patch.title {
            frontmatter.title = value;
        }
        if let Some(value) = patch.task_id {
            frontmatter.task_id = value;
        }
        if let Some(value) = patch.status {
            frontmatter.status = value;
        }
        if let Some(value) = patch.cwd {
            frontmatter.cwd = value;
        }
        if let Some(value) = patch.codex_session_id {
            frontmatter.codex_session_id = value;
        }
        if let Some(value) = patch.last_seen_at {
            frontmatter.last_seen_at = value;
        }
        if let Some(value) = patch.ended_at {
            frontmatter.ended_at = value;
        }
        if let Some(value) = patch.summary {
            summary = value;
        }

        self.write_record(&path, &frontmatter, &summary)
    }

    pub fn archive_session(&self, session_id: &str) -> Result<()> {
        let current = self
            .get_session(session_id)?
            .with_context(|| format!("codex session {session_id} not found"))?;
        let source = PathBuf::from(&current.path);
        let archive_dir = self
            .config
            .archive_dir()
            .join("codex_sessions")
            .join(&current.project_id);
        fs::create_dir_all(&archive_dir)
            .with_context(|| format!("failed creating {}", archive_dir.display()))?;

        let file_name = source
            .file_name()
            .with_context(|| format!("session path missing file name: {}", source.display()))?
            .to_string_lossy()
            .to_string();
        let mut target = archive_dir.join(&file_name);
        while target.exists() {
            target = archive_dir.join(format!("{}-{}", Ulid::new(), file_name));
        }

        fs::rename(&source, &target).with_context(|| {
            format!(
                "failed archiving codex session {} to {}",
                source.display(),
                target.display()
            )
        })?;
        Ok(())
    }

    pub fn normalize_statuses_on_boot(&self) -> Result<usize> {
        let mut updated = 0usize;
        for session in self.list_all_sessions_raw()? {
            if session.status != CodexSessionStatus::Live
                && session.status != CodexSessionStatus::Launching
            {
                continue;
            }

            self.update_session(
                &session.id,
                CodexSessionPatch {
                    status: Some(CodexSessionStatus::Resumable),
                    ended_at: Some(session.ended_at.or(Some(Utc::now()))),
                    last_seen_at: Some(Utc::now()),
                    ..Default::default()
                },
            )?;
            updated += 1;
        }
        Ok(updated)
    }

    fn project_dir(&self, project_id: &str) -> PathBuf {
        self.config.agents_dir().join(project_id)
    }

    fn session_path(&self, project_id: &str, session_id: &str) -> PathBuf {
        self.project_dir(project_id)
            .join(format!("{session_id}.md"))
    }

    fn read_record(&self, path: &Path) -> Result<CodexSessionRecord> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed reading {}", path.display()))?;
        parse_codex_session_markdown(&raw, path)
    }

    fn write_record(
        &self,
        path: &Path,
        frontmatter: &CodexSessionFrontmatter,
        summary: &str,
    ) -> Result<CodexSessionRecord> {
        let raw = render_codex_session_markdown(frontmatter, summary);
        fs::write(path, raw).with_context(|| format!("failed writing {}", path.display()))?;
        self.read_record(path)
    }
}

#[derive(Clone)]
pub struct CodexSessionManager {
    service: Arc<AppService>,
    store: CodexSessionStore,
    runtimes: Arc<Mutex<HashMap<String, Arc<LiveCodexSession>>>>,
}

impl CodexSessionManager {
    pub fn new(service: Arc<AppService>) -> Result<Self> {
        let store = CodexSessionStore::new(service.config.clone());
        let _ = store.normalize_statuses_on_boot()?;
        Ok(Self {
            service,
            store,
            runtimes: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn is_live(&self, session_id: &str) -> bool {
        self.runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_key(session_id)
    }

    pub fn load_session_transcript(
        &self,
        session_id: &str,
    ) -> Result<Vec<CodexTranscriptMessage>, ServiceError> {
        let record = self
            .store
            .get_session(session_id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("codex session {session_id} not found"))
            .map_err(ServiceError::Other)?;
        let record = self
            .attach_discovered_session_id(&record)
            .map_err(ServiceError::Other)?;
        let Some(codex_session_id) = record.codex_session_id.as_deref() else {
            return Ok(Vec::new());
        };
        load_codex_transcript_for_session_id(codex_session_id).map_err(ServiceError::Other)
    }

    fn take_runtime(&self, session_id: &str) -> Option<Arc<LiveCodexSession>> {
        self.runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id)
    }

    pub fn create_project_session(
        &self,
        project_id: &str,
        task_id: Option<String>,
    ) -> Result<CodexSessionRecord, ServiceError> {
        self.create_project_session_with_prompt(project_id, task_id, None)
    }

    pub fn create_project_session_with_prompt(
        &self,
        project_id: &str,
        task_id: Option<String>,
        prompt: Option<String>,
    ) -> Result<CodexSessionRecord, ServiceError> {
        let cwd = self.service.local_project_source_root(project_id)?;
        let title = self
            .service
            .suggest_codex_session_title(project_id, task_id.as_deref())?;
        let now = Utc::now();
        let record = self
            .store
            .create_session(NewCodexSession {
                project_id: project_id.to_string(),
                task_id: task_id.clone(),
                title,
                origin: CodexSessionOrigin::Topside,
                status: CodexSessionStatus::Launching,
                cwd: cwd.to_string_lossy().to_string(),
                codex_session_id: None,
                started_at: now,
                last_seen_at: now,
                ended_at: None,
                summary: build_summary_stub(None, task_id.as_deref(), now, "Launching"),
            })
            .map_err(ServiceError::Other)?;

        self.spawn_runtime(&record, prompt, None, false)?;
        self.spawn_codex_session_reconciler(record.id.clone(), cwd);
        self.store
            .get_session(&record.id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("codex session {} missing after launch", record.id))
            .map_err(ServiceError::Other)
    }

    pub fn assign_task_to_new_session(
        &self,
        task_id: &str,
        expected_revision: &str,
    ) -> Result<CodexSessionRecord, ServiceError> {
        let task = load_assignable_task_state(&self.service, task_id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("task {task_id} not found"))
            .map_err(ServiceError::Other)?;

        if task.revision != expected_revision {
            return Err(ServiceError::Conflict {
                expected: expected_revision.to_string(),
                current: task.revision,
            });
        }
        if task.status == TaskStatus::Done {
            return Err(anyhow::anyhow!("completed tasks cannot be sent to Codex").into());
        }
        if is_heading_title(&task.title) {
            return Err(anyhow::anyhow!("section headings cannot be sent to Codex").into());
        }

        self.service.local_project_source_root(&task.project_id)?;

        if let Some(existing) = find_active_task_session(&self.store, &task.project_id, task_id)
            .map_err(ServiceError::Other)?
        {
            return Ok(existing);
        }

        let prompt =
            self.service
                .build_codex_execute_prompt(&task.project_id, task_id, &task.title)?;
        let session = self.create_project_session_with_prompt(
            &task.project_id,
            Some(task_id.to_string()),
            Some(prompt),
        )?;

        let update_result = self.service.update_task(
            task_id,
            TaskPatch {
                assignee: Some(TASK_ASSIGNEE_CODEX.to_string()),
                status: Some(TaskStatus::InProgress),
                ..Default::default()
            },
            expected_revision,
            Actor::human("operator"),
        );
        if let Err(error) = update_result {
            if let Err(rollback_error) = self.archive_session(&session.id) {
                warn!(
                    error = %rollback_error,
                    session_id = %session.id,
                    task_id,
                    "failed rolling back codex session after task assignment failure"
                );
            }
            return Err(error);
        }

        self.store
            .get_session(&session.id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("codex session {} missing after assignment", session.id))
            .map_err(ServiceError::Other)
    }

    pub fn resume_session(&self, session_id: &str) -> Result<CodexSessionRecord, ServiceError> {
        if self.is_live(session_id) {
            return self
                .store
                .get_session(session_id)
                .map_err(ServiceError::Other)?
                .with_context(|| format!("codex session {session_id} not found"))
                .map_err(ServiceError::Other);
        }

        let record = self
            .store
            .get_session(session_id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("codex session {session_id} not found"))
            .map_err(ServiceError::Other)?;
        let record = self
            .attach_discovered_session_id(&record)
            .map_err(ServiceError::Other)?;
        let codex_session_id = record
            .codex_session_id
            .clone()
            .with_context(|| {
                format!(
                    "codex session {} has no persisted Codex thread id yet; it can only be resumed after Codex writes local history for it",
                    record.id
                )
            })
            .map_err(ServiceError::Other)?;
        self.spawn_runtime(&record, None, Some(codex_session_id.as_str()), true)?;
        let session = self
            .store
            .update_session(
                &record.id,
                CodexSessionPatch {
                    status: Some(CodexSessionStatus::Live),
                    ended_at: Some(None),
                    last_seen_at: Some(Utc::now()),
                    ..Default::default()
                },
            )
            .map_err(ServiceError::Other)?;
        Ok(session)
    }

    fn attach_discovered_session_id(
        &self,
        record: &CodexSessionRecord,
    ) -> Result<CodexSessionRecord> {
        if record.codex_session_id.is_some() {
            return Ok(record.clone());
        }

        let canonical_cwd = record.cwd.as_str();
        let canonical_cwd = Path::new(canonical_cwd)
            .canonicalize()
            .with_context(|| format!("failed canonicalizing {}", record.cwd))?;
        let history = discover_codex_history_for_root(&canonical_cwd)?;
        let existing_sessions = self.store.list_all_sessions()?;
        let Some(found) = select_codex_launch_history_candidate(
            record,
            &canonical_cwd,
            &history,
            &existing_sessions,
        ) else {
            return Ok(record.clone());
        };

        let title = if record.title == "New Codex session" && !found.thread_name.trim().is_empty() {
            Some(found.thread_name.clone())
        } else {
            None
        };
        self.store.update_session(
            &record.id,
            CodexSessionPatch {
                title,
                codex_session_id: Some(Some(found.codex_session_id.clone())),
                last_seen_at: Some(found.updated_at),
                ..Default::default()
            },
        )?;

        self.store.get_session(&record.id)?.with_context(|| {
            format!(
                "codex session {} missing after attaching history",
                record.id
            )
        })
    }

    pub fn terminate_session(&self, session_id: &str) -> Result<(), ServiceError> {
        let runtime = {
            self.runtimes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(session_id)
                .cloned()
        };
        let runtime = runtime
            .with_context(|| format!("codex session {session_id} is not live"))
            .map_err(ServiceError::Other)?;
        let now = Utc::now();
        let session = self
            .store
            .update_session(
                session_id,
                CodexSessionPatch {
                    status: Some(CodexSessionStatus::Resumable),
                    ended_at: Some(Some(now)),
                    last_seen_at: Some(now),
                    ..Default::default()
                },
            )
            .map_err(ServiceError::Other)?;
        runtime.push_message(CodexTerminalServerMessage::Status {
            status: CodexSessionStatus::Resumable.as_str().to_string(),
        });
        runtime.terminate().map_err(ServiceError::Other)?;
        if let Err(error) =
            release_linked_task_assignment_if_idle(&self.service, &self.store, &session)
        {
            warn!(
                error = %error,
                session_id,
                "failed releasing linked task assignment on terminate"
            );
        }
        Ok(())
    }

    pub fn restart_session(&self, session_id: &str) -> Result<CodexSessionRecord, ServiceError> {
        let record = self
            .store
            .get_session(session_id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("codex session {session_id} not found"))
            .map_err(ServiceError::Other)?;

        let runtime = self.take_runtime(session_id);
        if let Some(runtime) = runtime {
            if let Err(error) = runtime.terminate() {
                warn!(
                    error = %error,
                    session_id,
                    "failed terminating codex session during restart; continuing with restart"
                );
            }
        }

        let now = Utc::now();
        let resume_session_id = record.codex_session_id.clone();
        let restarts_existing_thread = resume_session_id.is_some();
        let next_status = if restarts_existing_thread {
            CodexSessionStatus::Live
        } else {
            CodexSessionStatus::Launching
        };
        let summary = if restarts_existing_thread {
            None
        } else {
            Some(build_summary_stub(
                Some(record.title.as_str()),
                record.task_id.as_deref(),
                record.started_at,
                "Launching",
            ))
        };

        self.store
            .update_session(
                session_id,
                CodexSessionPatch {
                    status: Some(next_status),
                    ended_at: Some(None),
                    last_seen_at: Some(now),
                    summary,
                    ..Default::default()
                },
            )
            .map_err(ServiceError::Other)?;

        self.spawn_runtime(
            &record,
            None,
            resume_session_id.as_deref(),
            restarts_existing_thread,
        )?;
        if !restarts_existing_thread {
            self.spawn_codex_session_reconciler(record.id.clone(), PathBuf::from(&record.cwd));
        }

        let restarted = self
            .store
            .get_session(&record.id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("codex session {} missing after restart", record.id))
            .map_err(ServiceError::Other)?;
        Ok(restarted)
    }

    pub fn archive_session(&self, session_id: &str) -> Result<(), ServiceError> {
        let record = self
            .store
            .get_session(session_id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("codex session {session_id} not found"))
            .map_err(ServiceError::Other)?;

        let runtime = self.take_runtime(session_id);
        let should_normalize_exit = runtime.is_some()
            || record.status == CodexSessionStatus::Live
            || record.status == CodexSessionStatus::Launching;

        if should_normalize_exit {
            let now = Utc::now();
            self.store
                .update_session(
                    session_id,
                    CodexSessionPatch {
                        status: Some(CodexSessionStatus::Resumable),
                        ended_at: Some(Some(now)),
                        last_seen_at: Some(now),
                        ..Default::default()
                    },
                )
                .map_err(ServiceError::Other)?;
        }

        if let Some(runtime) = runtime {
            runtime.push_message(CodexTerminalServerMessage::Status {
                status: CodexSessionStatus::Resumable.as_str().to_string(),
            });
            if let Err(error) = runtime.terminate() {
                warn!(
                    error = %error,
                    session_id,
                    "failed terminating codex session during archive; continuing with archive"
                );
            }
        }

        self.store
            .archive_session(session_id)
            .map_err(ServiceError::Other)?;
        if let Err(error) =
            release_linked_task_assignment_if_idle(&self.service, &self.store, &record)
        {
            warn!(
                error = %error,
                session_id,
                "failed releasing linked task assignment on archive"
            );
        }
        Ok(())
    }

    pub fn subscribe(
        &self,
        session_id: &str,
    ) -> Result<(Vec<String>, broadcast::Receiver<CodexTerminalServerMessage>), ServiceError> {
        let runtime = {
            self.runtimes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(session_id)
                .cloned()
        };
        let runtime = runtime
            .with_context(|| format!("codex session {session_id} is not live"))
            .map_err(ServiceError::Other)?;
        Ok((runtime.backlog(), runtime.subscribe()))
    }

    pub fn send_input(&self, session_id: &str, data: &str) -> Result<(), ServiceError> {
        let runtime = {
            self.runtimes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(session_id)
                .cloned()
        };
        let runtime = runtime
            .with_context(|| format!("codex session {session_id} is not live"))
            .map_err(ServiceError::Other)?;
        runtime.write_input(data).map_err(ServiceError::Other)
    }

    pub fn resize(&self, session_id: &str, rows: u16, cols: u16) -> Result<(), ServiceError> {
        let runtime = {
            self.runtimes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(session_id)
                .cloned()
        };
        let runtime = runtime
            .with_context(|| format!("codex session {session_id} is not live"))
            .map_err(ServiceError::Other)?;
        runtime.resize(rows, cols).map_err(ServiceError::Other)
    }

    fn spawn_runtime(
        &self,
        record: &CodexSessionRecord,
        prompt: Option<String>,
        resume_session_id: Option<&str>,
        mark_live_immediately: bool,
    ) -> Result<(), ServiceError> {
        let cwd = PathBuf::from(&record.cwd);
        let binary = codex_binary();
        let command = build_codex_command(
            &binary,
            &cwd,
            &self.service.config.workspace_root,
            prompt.as_deref(),
            resume_session_id,
        );
        info!(
            session_id = %record.id,
            project_id = %record.project_id,
            cwd = %cwd.display(),
            binary,
            resume_session_id,
            mark_live_immediately,
            "starting codex session"
        );

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: DEFAULT_PTY_ROWS,
                cols: DEFAULT_PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed allocating pty")
            .map_err(ServiceError::Other)?;
        let child = pty
            .slave
            .spawn_command(command)
            .context("failed spawning codex process")
            .map_err(ServiceError::Other)?;

        let writer = pty
            .master
            .take_writer()
            .context("failed opening pty writer")
            .map_err(ServiceError::Other)?;
        let reader = pty
            .master
            .try_clone_reader()
            .context("failed opening pty reader")
            .map_err(ServiceError::Other)?;

        let runtime = Arc::new(LiveCodexSession::new(pty.master, writer, child));
        self.spawn_output_pump(record.id.clone(), runtime.clone(), reader);
        self.spawn_exit_watcher(record.id.clone(), runtime.clone());

        self.runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(record.id.clone(), runtime);

        if mark_live_immediately {
            self.store
                .update_session(
                    &record.id,
                    CodexSessionPatch {
                        status: Some(CodexSessionStatus::Live),
                        ended_at: Some(None),
                        last_seen_at: Some(Utc::now()),
                        ..Default::default()
                    },
                )
                .map_err(ServiceError::Other)?;
        }

        Ok(())
    }

    fn spawn_output_pump(
        &self,
        session_id: String,
        runtime: Arc<LiveCodexSession>,
        mut reader: Box<dyn Read + Send>,
    ) {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(read) => {
                        let data = String::from_utf8_lossy(&buf[..read]).to_string();
                        runtime.push_output(data);
                    }
                    Err(error) => {
                        runtime.push_message(CodexTerminalServerMessage::Error {
                            message: format!("terminal read failed for {session_id}: {error}"),
                        });
                        break;
                    }
                }
            }
        });
    }

    fn spawn_exit_watcher(&self, session_id: String, runtime: Arc<LiveCodexSession>) {
        let runtimes = Arc::clone(&self.runtimes);
        let store = self.store.clone();
        let service = Arc::clone(&self.service);
        std::thread::spawn(move || {
            let wait_result = runtime.wait();
            let ended_at = Utc::now();
            let should_persist_exit = {
                let mut guard = runtimes.lock().unwrap_or_else(|poison| poison.into_inner());
                match guard.get(&session_id) {
                    Some(current) if Arc::ptr_eq(current, &runtime) => {
                        guard.remove(&session_id);
                        true
                    }
                    _ => false,
                }
            };
            if !should_persist_exit {
                return;
            }
            if let Err(error) = wait_result {
                runtime.push_message(CodexTerminalServerMessage::Error {
                    message: format!("codex session exited with an error: {error}"),
                });
                warn!(error = %error, session_id, "codex session wait failed");
            } else {
                info!(session_id, "codex session exited");
            }
            if let Err(error) = store.update_session(
                &session_id,
                CodexSessionPatch {
                    status: Some(CodexSessionStatus::Resumable),
                    ended_at: Some(Some(ended_at)),
                    last_seen_at: Some(ended_at),
                    ..Default::default()
                },
            ) {
                if matches!(store.get_session(&session_id), Ok(None)) {
                    return;
                }
                warn!(error = %error, session_id, "failed persisting codex session exit");
            } else if let Ok(Some(session)) = store.get_session(&session_id) {
                if let Err(error) =
                    release_linked_task_assignment_if_idle(&service, &store, &session)
                {
                    warn!(
                        error = %error,
                        session_id,
                        "failed releasing linked task assignment after session exit"
                    );
                }
            }
            runtime.push_message(CodexTerminalServerMessage::Status {
                status: CodexSessionStatus::Resumable.as_str().to_string(),
            });
        });
    }

    fn spawn_codex_session_reconciler(&self, session_id: String, cwd: PathBuf) {
        let store = self.store.clone();
        let runtimes = Arc::clone(&self.runtimes);
        std::thread::spawn(move || {
            let started = Instant::now();
            while started.elapsed() < CODEX_RECONCILE_TIMEOUT {
                std::thread::sleep(CODEX_RECONCILE_POLL_INTERVAL);
                if !runtimes
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .contains_key(&session_id)
                {
                    return;
                }

                let Ok(canonical_cwd) = cwd.canonicalize() else {
                    continue;
                };
                let current = match store.get_session(&session_id) {
                    Ok(Some(record)) => record,
                    Ok(None) => return,
                    Err(error) => {
                        warn!(error = %error, session_id, "failed reloading launching codex session");
                        return;
                    }
                };
                let Ok(history) = discover_codex_history_for_root(&canonical_cwd) else {
                    continue;
                };
                let existing_sessions = match store.list_all_sessions() {
                    Ok(sessions) => sessions,
                    Err(error) => {
                        warn!(error = %error, session_id, "failed listing codex sessions during launch reconciliation");
                        continue;
                    }
                };
                // Avoid attaching a new launch to an older thread from the same cwd.
                let Some(found) = select_codex_launch_history_candidate(
                    &current,
                    &canonical_cwd,
                    &history,
                    &existing_sessions,
                ) else {
                    continue;
                };
                let title = if current.title == "New Codex session"
                    && !found.thread_name.trim().is_empty()
                {
                    Some(found.thread_name.clone())
                } else {
                    None
                };
                if let Err(error) = store.update_session(
                    &session_id,
                    CodexSessionPatch {
                        title,
                        codex_session_id: Some(Some(found.codex_session_id.clone())),
                        status: Some(CodexSessionStatus::Live),
                        last_seen_at: Some(found.updated_at),
                        summary: Some(build_summary_stub(
                            Some(found.thread_name.as_str()),
                            current.task_id.as_deref(),
                            current.started_at,
                            "Live",
                        )),
                        ..Default::default()
                    },
                ) {
                    warn!(error = %error, session_id, "failed attaching codex session id");
                }
                return;
            }

            if let Err(error) = store.update_session(
                &session_id,
                CodexSessionPatch {
                    status: Some(CodexSessionStatus::Live),
                    last_seen_at: Some(Utc::now()),
                    ..Default::default()
                },
            ) {
                warn!(error = %error, session_id, "failed promoting launching codex session");
            }
        });
    }
}

pub fn reconcile_project_codex_history(
    store: &CodexSessionStore,
    project_id: &str,
    project_root: &Path,
) -> Result<usize> {
    let history = discover_codex_history_for_root(project_root)?;
    if history.is_empty() {
        return Ok(0);
    }

    let mut sessions = keep_topside_codex_sessions(dedupe_codex_sessions(
        store.list_project_sessions_raw(project_id)?,
    ));
    sessions.sort_by(|left, right| {
        right
            .started_at
            .cmp(&left.started_at)
            .then(right.last_seen_at.cmp(&left.last_seen_at))
            .then(left.id.cmp(&right.id))
    });

    let mut existing_sessions = store.list_all_sessions_raw()?;
    let mut reconciled = 0usize;

    for session in sessions {
        if session.codex_session_id.is_some() {
            continue;
        }

        let canonical_cwd = match Path::new(&session.cwd).canonicalize() {
            Ok(path) => path,
            Err(_) => continue,
        };
        let Some(found) = select_codex_launch_history_candidate(
            &session,
            &canonical_cwd,
            &history,
            &existing_sessions,
        ) else {
            continue;
        };

        let title = if session.title == "New Codex session" && !found.thread_name.trim().is_empty()
        {
            Some(found.thread_name.clone())
        } else {
            None
        };
        let last_seen_at = if found.updated_at > session.last_seen_at {
            found.updated_at
        } else {
            session.last_seen_at
        };
        let updated = store.update_session(
            &session.id,
            CodexSessionPatch {
                title,
                codex_session_id: Some(Some(found.codex_session_id.clone())),
                last_seen_at: Some(last_seen_at),
                ..Default::default()
            },
        )?;

        if let Some(existing) = existing_sessions
            .iter_mut()
            .find(|existing| existing.id == updated.id)
        {
            *existing = updated;
        }
        reconciled += 1;
    }

    Ok(reconciled)
}

struct LiveCodexSession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn Child + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    tx: broadcast::Sender<CodexTerminalServerMessage>,
    backlog: Arc<Mutex<TerminalBacklog>>,
}

impl LiveCodexSession {
    fn new(
        master: Box<dyn MasterPty + Send>,
        writer: Box<dyn Write + Send>,
        child: Box<dyn Child + Send>,
    ) -> Self {
        let killer = child.clone_killer();
        let (tx, _) = broadcast::channel(512);
        Self {
            master: Arc::new(Mutex::new(master)),
            writer: Arc::new(Mutex::new(writer)),
            child: Arc::new(Mutex::new(child)),
            killer: Arc::new(Mutex::new(killer)),
            tx,
            backlog: Arc::new(Mutex::new(TerminalBacklog::default())),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<CodexTerminalServerMessage> {
        self.tx.subscribe()
    }

    fn backlog(&self) -> Vec<String> {
        self.backlog
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .to_vec()
    }

    fn push_output(&self, data: String) {
        {
            let mut backlog = self
                .backlog
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            backlog.push(data.clone());
        }
        let _ = self.tx.send(CodexTerminalServerMessage::Output { data });
    }

    fn push_message(&self, message: CodexTerminalServerMessage) {
        let _ = self.tx.send(message);
    }

    fn write_input(&self, data: &str) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        writer
            .write_all(data.as_bytes())
            .context("failed writing terminal input")?;
        writer.flush().context("failed flushing terminal input")?;
        Ok(())
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed resizing pty")?;
        Ok(())
    }

    fn terminate(&self) -> Result<()> {
        self.killer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .kill()
            .context("failed terminating codex session")
    }

    fn wait(&self) -> Result<()> {
        self.child
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .wait()
            .context("failed waiting on codex session")?;
        Ok(())
    }
}

#[derive(Default)]
struct TerminalBacklog {
    chunks: VecDeque<String>,
    byte_len: usize,
}

impl TerminalBacklog {
    fn to_vec(&self) -> Vec<String> {
        self.chunks.iter().cloned().collect()
    }

    fn push(&mut self, data: String) {
        self.byte_len += data.len();
        self.chunks.push_back(data);
        while self.byte_len > OUTPUT_BACKLOG_BYTES && self.chunks.len() > 1 {
            if let Some(removed) = self.chunks.pop_front() {
                self.byte_len = self.byte_len.saturating_sub(removed.len());
            }
        }
    }
}

pub fn parse_codex_session_markdown(content: &str, path: &Path) -> Result<CodexSessionRecord> {
    let (frontmatter_raw, body) = split_frontmatter(content)?;
    let frontmatter: CodexSessionFrontmatter = serde_yaml::from_str(&frontmatter_raw)
        .context("failed parsing codex session frontmatter")?;
    if frontmatter.session_type != CODEX_SESSION_TYPE {
        anyhow::bail!(
            "unsupported codex session type: {}",
            frontmatter.session_type
        );
    }
    Ok(CodexSessionRecord {
        id: frontmatter.id.clone(),
        project_id: frontmatter.project_id.clone(),
        task_id: frontmatter.task_id.clone(),
        title: frontmatter.title.clone(),
        origin: frontmatter.origin.clone(),
        status: frontmatter.status.clone(),
        cwd: frontmatter.cwd.clone(),
        codex_session_id: frontmatter.codex_session_id.clone(),
        started_at: frontmatter.started_at,
        last_seen_at: frontmatter.last_seen_at,
        ended_at: frontmatter.ended_at,
        summary: body,
        path: path.to_string_lossy().to_string(),
    })
}

pub fn render_codex_session_markdown(
    frontmatter: &CodexSessionFrontmatter,
    summary: &str,
) -> String {
    let mut yaml = String::new();
    yaml.push_str(&format!("id: {}\n", yaml_scalar(&frontmatter.id)));
    yaml.push_str(&format!("type: {}\n", CODEX_SESSION_TYPE));
    yaml.push_str(&format!(
        "project_id: {}\n",
        yaml_scalar(&frontmatter.project_id)
    ));
    if let Some(task_id) = &frontmatter.task_id {
        yaml.push_str(&format!("task_id: {}\n", yaml_scalar(task_id)));
    }
    yaml.push_str(&format!("title: {}\n", yaml_scalar(&frontmatter.title)));
    yaml.push_str(&format!("origin: {}\n", frontmatter.origin.as_str()));
    yaml.push_str(&format!("status: {}\n", frontmatter.status.as_str()));
    yaml.push_str(&format!("cwd: {}\n", yaml_scalar(&frontmatter.cwd)));
    if let Some(codex_session_id) = &frontmatter.codex_session_id {
        yaml.push_str(&format!(
            "codex_session_id: {}\n",
            yaml_scalar(codex_session_id)
        ));
    }
    yaml.push_str(&format!(
        "started_at: {}\n",
        frontmatter.started_at.to_rfc3339()
    ));
    yaml.push_str(&format!(
        "last_seen_at: {}\n",
        frontmatter.last_seen_at.to_rfc3339()
    ));
    if let Some(ended_at) = frontmatter.ended_at {
        yaml.push_str(&format!("ended_at: {}\n", ended_at.to_rfc3339()));
    }

    let summary = if summary.ends_with('\n') {
        summary.to_string()
    } else {
        format!("{summary}\n")
    };

    format!("---\n{yaml}---\n{summary}")
}

pub fn build_summary_stub(
    thread_name: Option<&str>,
    task_id: Option<&str>,
    started_at: DateTime<Utc>,
    status_label: &str,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("# {}", thread_name.unwrap_or("Codex session")));
    lines.push(String::new());
    lines.push(format!("- Status: {status_label}"));
    lines.push(format!("- Started: {}", started_at.to_rfc3339()));
    if let Some(task_id) = task_id {
        lines.push(format!("- Linked task: {task_id}"));
    }
    lines.push(String::new());
    lines.push("Summary pending.".to_string());
    lines.join("\n")
}

fn is_active_codex_status(status: &CodexSessionStatus) -> bool {
    matches!(
        status,
        CodexSessionStatus::Launching | CodexSessionStatus::Live
    )
}

fn load_assignable_task_state(
    service: &AppService,
    task_id: &str,
) -> Result<Option<AssignableTaskState>> {
    let Some((_record, parsed)) = service.db.parse_entity_from_disk(task_id)? else {
        return Ok(None);
    };
    let revision = parsed.revision;
    match parsed.frontmatter {
        EntityFrontmatter::Task(task) => Ok(Some(AssignableTaskState {
            project_id: task.project_id,
            title: task.title,
            status: task.status,
            assignee: task.assignee,
            revision,
        })),
        _ => anyhow::bail!("entity {task_id} is not a task"),
    }
}

fn find_active_task_session(
    store: &CodexSessionStore,
    project_id: &str,
    task_id: &str,
) -> Result<Option<CodexSessionRecord>> {
    Ok(store
        .list_project_sessions(project_id)?
        .into_iter()
        .find(|session| {
            session.task_id.as_deref() == Some(task_id) && is_active_codex_status(&session.status)
        }))
}

fn release_linked_task_assignment_if_idle(
    service: &AppService,
    store: &CodexSessionStore,
    session: &CodexSessionRecord,
) -> Result<()> {
    let Some(task_id) = session.task_id.as_deref() else {
        return Ok(());
    };

    let has_other_active_assignment = store
        .list_project_sessions(&session.project_id)?
        .into_iter()
        .any(|candidate| {
            candidate.id != session.id
                && candidate.task_id.as_deref() == Some(task_id)
                && is_active_codex_status(&candidate.status)
        });
    if has_other_active_assignment {
        return Ok(());
    }

    let Some(task) = load_assignable_task_state(service, task_id)? else {
        return Ok(());
    };
    if task.assignee == TASK_ASSIGNEE_UNASSIGNED {
        return Ok(());
    }

    service.update_task(
        task_id,
        TaskPatch {
            assignee: Some(TASK_ASSIGNEE_UNASSIGNED.to_string()),
            ..Default::default()
        },
        &task.revision,
        Actor::agent("topside"),
    )?;
    Ok(())
}

fn select_codex_launch_history_candidate<'a>(
    current: &CodexSessionRecord,
    canonical_cwd: &Path,
    history: &'a [CodexHistorySession],
    existing_sessions: &[CodexSessionRecord],
) -> Option<&'a CodexHistorySession> {
    let launch_cutoff =
        current.started_at - ChronoDuration::seconds(CODEX_HISTORY_MATCH_GRACE_SECONDS);
    history.iter().find(|item| {
        if item.cwd.as_path() != canonical_cwd || item.updated_at < launch_cutoff {
            return false;
        }
        !existing_sessions.iter().any(|session| {
            session.id != current.id
                && session.codex_session_id.as_deref() == Some(item.codex_session_id.as_str())
        })
    })
}

pub fn discover_codex_history_for_root(project_root: &Path) -> Result<Vec<CodexHistorySession>> {
    let canonical_root = project_root
        .canonicalize()
        .with_context(|| format!("failed canonicalizing {}", project_root.display()))?;
    let codex_home = codex_home_dir()?;
    let mut discovered_by_id = HashMap::<String, CodexHistorySession>::new();

    for session in discover_codex_history_from_thread_store(&canonical_root, &codex_home)? {
        discovered_by_id.insert(session.codex_session_id.clone(), session);
    }
    for session in discover_codex_history_from_session_index(&canonical_root, &codex_home)? {
        match discovered_by_id.entry(session.codex_session_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(session);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                if existing.thread_name.trim().is_empty() && !session.thread_name.trim().is_empty()
                {
                    existing.thread_name = session.thread_name;
                }
                if session.updated_at > existing.updated_at {
                    existing.updated_at = session.updated_at;
                }
                if session.started_at < existing.started_at {
                    existing.started_at = session.started_at;
                }
            }
        }
    }

    let mut discovered = discovered_by_id.into_values().collect::<Vec<_>>();
    discovered.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(discovered)
}

fn discover_codex_history_from_session_index(
    canonical_root: &Path,
    codex_home: &Path,
) -> Result<Vec<CodexHistorySession>> {
    let session_index_path = codex_home.join("session_index.jsonl");
    if !session_index_path.exists() {
        return Ok(Vec::new());
    }

    let file_map = discover_session_file_map(&codex_home.join("sessions"))?;
    let reader = BufReader::new(
        fs::File::open(&session_index_path)
            .with_context(|| format!("failed opening {}", session_index_path.display()))?,
    );

    let mut discovered = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let Some(index_entry) = parse_session_index_entry(&line)? else {
            continue;
        };
        let Some(path) = file_map.get(&index_entry.id) else {
            continue;
        };
        let Some(meta) = parse_session_meta(path)? else {
            continue;
        };
        let canonical_cwd = match meta.cwd.canonicalize() {
            Ok(cwd) => cwd,
            Err(_) => continue,
        };
        if canonical_cwd == canonical_root || canonical_cwd.starts_with(canonical_root) {
            discovered.push(CodexHistorySession {
                codex_session_id: index_entry.id,
                thread_name: index_entry.thread_name,
                cwd: canonical_cwd,
                started_at: meta.started_at,
                updated_at: index_entry.updated_at,
            });
        }
    }

    Ok(discovered)
}

pub fn codex_home_dir() -> Result<PathBuf> {
    if let Some(value) = env::var_os("TOPSIDE_CODEX_HOME") {
        return Ok(PathBuf::from(value));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn append_codex_args(
    command: &mut CommandBuilder,
    cwd: &Path,
    workspace_root: &Path,
    prompt: Option<&str>,
    resume_session_id: Option<&str>,
) {
    command.arg("-C");
    command.arg(cwd.to_string_lossy().to_string());
    for server_name in configured_codex_mcp_server_names() {
        if server_name == "topside" {
            continue;
        }
        command.arg("-c");
        command.arg(format!(
            "mcp_servers.{}.enabled=false",
            codex_config_key_segment(&server_name)
        ));
    }
    command.arg("-c");
    command.arg("check_for_update_on_startup=false");
    command.arg("-c");
    command.arg(format!(
        "mcp_servers.topside.command={}",
        toml_basic_string(
            &env::current_exe()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_else(|_| "topside".to_string())
        )
    ));
    command.arg("-c");
    command.arg(format!(
        "mcp_servers.topside.args=[{}, {}, {}]",
        toml_basic_string("--workspace"),
        toml_basic_string(&workspace_root.to_string_lossy()),
        toml_basic_string("mcp")
    ));
    if let Some(session_id) = resume_session_id {
        command.arg("resume");
        command.arg(session_id.to_string());
    }
    if let Some(prompt) = prompt {
        command.arg(prompt.to_string());
    }
}

fn build_codex_command(
    binary: &str,
    cwd: &Path,
    workspace_root: &Path,
    prompt: Option<&str>,
    resume_session_id: Option<&str>,
) -> CommandBuilder {
    let mut command = CommandBuilder::new(binary);
    command.cwd(cwd.as_os_str());
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "Topside");
    append_codex_args(&mut command, cwd, workspace_root, prompt, resume_session_id);
    command
}

fn codex_binary() -> String {
    env::var("TOPSIDE_CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
}

fn configured_codex_mcp_server_names() -> Vec<String> {
    let config_path = match codex_home_dir() {
        Ok(path) => path.join("config.toml"),
        Err(_) => return Vec::new(),
    };
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    let value = match toml::from_str::<toml::Value>(&raw) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let Some(table) = value.get("mcp_servers").and_then(|entry| entry.as_table()) else {
        return Vec::new();
    };
    table.keys().cloned().collect()
}

fn codex_config_key_segment(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        value.to_string()
    } else {
        toml_basic_string(value)
    }
}

fn toml_basic_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn discover_session_file_map(sessions_root: &Path) -> Result<HashMap<String, PathBuf>> {
    let mut file_map = HashMap::new();
    if !sessions_root.exists() {
        return Ok(file_map);
    }

    let file_name_re = Regex::new(
        r"([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})\.jsonl$",
    )
    .expect("valid session file regex");
    for entry in WalkDir::new(sessions_root) {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        let Some(captures) = file_name_re.captures(&file_name) else {
            continue;
        };
        let Some(session_id) = captures.get(1) else {
            continue;
        };
        file_map.insert(session_id.as_str().to_string(), entry.path().to_path_buf());
    }
    Ok(file_map)
}

fn load_codex_transcript_for_session_id(
    codex_session_id: &str,
) -> Result<Vec<CodexTranscriptMessage>> {
    let codex_home = codex_home_dir()?;
    let Some(path) = find_rollout_path_for_codex_session_id(&codex_home, codex_session_id)? else {
        return Ok(Vec::new());
    };
    parse_codex_rollout_transcript(&path)
}

fn find_rollout_path_for_codex_session_id(
    codex_home: &Path,
    codex_session_id: &str,
) -> Result<Option<PathBuf>> {
    let file_map = discover_session_file_map(&codex_home.join("sessions"))?;
    Ok(file_map.get(codex_session_id).cloned())
}

fn parse_codex_rollout_transcript(path: &Path) -> Result<Vec<CodexTranscriptMessage>> {
    let event_messages = parse_codex_rollout_messages(path, parse_codex_event_transcript_message)?;
    if !event_messages.is_empty() {
        return Ok(event_messages);
    }
    parse_codex_rollout_messages(path, parse_codex_response_transcript_message)
}

fn parse_codex_rollout_messages<F>(
    path: &Path,
    mut parser: F,
) -> Result<Vec<CodexTranscriptMessage>>
where
    F: FnMut(&Value) -> Option<CodexTranscriptMessage>,
{
    let reader = BufReader::new(
        fs::File::open(path).with_context(|| format!("failed opening {}", path.display()))?,
    );
    let mut messages = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).context("invalid codex session json")?;
        if let Some(message) = parser(&value) {
            messages.push(message);
        }
    }
    Ok(messages)
}

fn parse_codex_event_transcript_message(value: &Value) -> Option<CodexTranscriptMessage> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?.as_object()?;
    let role = match payload.get("type").and_then(Value::as_str)? {
        "user_message" => CodexTranscriptRole::User,
        "agent_message" => CodexTranscriptRole::Assistant,
        _ => return None,
    };
    let text = payload.get("message").and_then(Value::as_str)?.trim();
    if text.is_empty() {
        return None;
    }
    Some(CodexTranscriptMessage {
        role,
        text: text.to_string(),
        timestamp: parse_datetime_value(value.get("timestamp")).ok(),
    })
}

fn parse_codex_response_transcript_message(value: &Value) -> Option<CodexTranscriptMessage> {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?.as_object()?;
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let role = match payload.get("role").and_then(Value::as_str)? {
        "user" => CodexTranscriptRole::User,
        "assistant" => CodexTranscriptRole::Assistant,
        _ => return None,
    };
    let content = payload.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter_map(|item| {
            let item_type = item.get("type").and_then(Value::as_str)?;
            match item_type {
                "input_text" | "output_text" => item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(ToString::to_string),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.is_empty() || is_bootstrap_transcript_message(&role, &text) {
        return None;
    }
    Some(CodexTranscriptMessage {
        role,
        text,
        timestamp: parse_datetime_value(value.get("timestamp")).ok(),
    })
}

fn is_bootstrap_transcript_message(role: &CodexTranscriptRole, text: &str) -> bool {
    matches!(role, CodexTranscriptRole::User)
        && (text.starts_with("# AGENTS.md instructions")
            || text.starts_with("<environment_context>")
            || text.starts_with("<permissions instructions>"))
}

fn parse_session_index_entry(line: &str) -> Result<Option<SessionIndexEntry>> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(line).context("invalid session index json")?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .context("session index entry missing id")?;
    let thread_name = value
        .get("thread_name")
        .and_then(Value::as_str)
        .unwrap_or("Codex session")
        .to_string();
    let updated_at = parse_datetime_value(value.get("updated_at"))
        .context("session index entry missing updated_at")?;
    Ok(Some(SessionIndexEntry {
        id,
        thread_name,
        updated_at,
    }))
}

fn parse_session_meta(path: &Path) -> Result<Option<SessionMeta>> {
    let reader = BufReader::new(
        fs::File::open(path).with_context(|| format!("failed opening {}", path.display()))?,
    );
    for line in reader.lines().take(40) {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).context("invalid codex session json")?;
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = value
            .get("payload")
            .and_then(Value::as_object)
            .context("session_meta payload missing")?;
        let cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .context("session_meta payload missing cwd")?;
        let started_at = parse_datetime_value(payload.get("timestamp"))
            .or_else(|_| parse_datetime_value(value.get("timestamp")))
            .context("session_meta missing timestamp")?;
        return Ok(Some(SessionMeta { cwd, started_at }));
    }
    Ok(None)
}

fn parse_datetime_value(value: Option<&Value>) -> Result<DateTime<Utc>> {
    let Some(value) = value.and_then(Value::as_str) else {
        anyhow::bail!("missing datetime string")
    };
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .with_context(|| format!("invalid RFC3339 datetime: {value}"))
}

fn discover_codex_history_from_thread_store(
    canonical_root: &Path,
    codex_home: &Path,
) -> Result<Vec<CodexHistorySession>> {
    let db_path = codex_home.join("state_5.sqlite");
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let connection = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed opening {}", db_path.display()))?;
    let mut statement = connection.prepare(
        "SELECT id, cwd, created_at, updated_at
         FROM threads
         WHERE archived = 0 AND source != 'exec'",
    )?;

    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    let mut discovered = Vec::new();
    for row in rows {
        let (codex_session_id, cwd, created_at, updated_at) = row?;
        let canonical_cwd = match PathBuf::from(cwd).canonicalize() {
            Ok(cwd) => cwd,
            Err(_) => continue,
        };
        if canonical_cwd != canonical_root && !canonical_cwd.starts_with(canonical_root) {
            continue;
        }
        discovered.push(CodexHistorySession {
            codex_session_id,
            thread_name: String::new(),
            cwd: canonical_cwd,
            started_at: parse_unix_timestamp(created_at)
                .context("thread store entry missing created_at")?,
            updated_at: parse_unix_timestamp(updated_at)
                .context("thread store entry missing updated_at")?,
        });
    }

    Ok(discovered)
}

fn parse_unix_timestamp(value: i64) -> Result<DateTime<Utc>> {
    Utc.timestamp_opt(value, 0)
        .single()
        .with_context(|| format!("invalid unix timestamp: {value}"))
}

fn sort_sessions(sessions: &mut [CodexSessionRecord]) {
    sessions.sort_by(|left, right| {
        session_sort_weight(&left.status)
            .cmp(&session_sort_weight(&right.status))
            .then(right.last_seen_at.cmp(&left.last_seen_at))
            .then(left.title.cmp(&right.title))
    });
}

fn dedupe_codex_sessions(sessions: Vec<CodexSessionRecord>) -> Vec<CodexSessionRecord> {
    let mut canonical_by_codex_id = HashMap::<String, CodexSessionRecord>::new();
    let mut passthrough = Vec::new();

    for session in sessions {
        let Some(codex_session_id) = session.codex_session_id.clone() else {
            passthrough.push(session);
            continue;
        };
        match canonical_by_codex_id.entry(codex_session_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(session);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if codex_session_record_preference(&session)
                    > codex_session_record_preference(entry.get())
                {
                    entry.insert(session);
                }
            }
        }
    }

    passthrough.extend(canonical_by_codex_id.into_values());
    passthrough
}

fn keep_topside_codex_sessions(sessions: Vec<CodexSessionRecord>) -> Vec<CodexSessionRecord> {
    sessions
        .into_iter()
        .filter(|session| session.origin == CodexSessionOrigin::Topside)
        .collect()
}

fn codex_session_record_preference(
    session: &CodexSessionRecord,
) -> (
    std::cmp::Reverse<u8>,
    bool,
    bool,
    DateTime<Utc>,
    DateTime<Utc>,
    &str,
) {
    (
        std::cmp::Reverse(session_sort_weight(&session.status)),
        session.origin == CodexSessionOrigin::Topside,
        session.title != "New Codex session",
        session.last_seen_at,
        session.started_at,
        session.id.as_str(),
    )
}

fn session_sort_weight(status: &CodexSessionStatus) -> u8 {
    match status {
        CodexSessionStatus::Live => 0,
        CodexSessionStatus::Launching => 1,
        CodexSessionStatus::Resumable => 2,
    }
}

fn yaml_scalar(value: &str) -> String {
    let encoded = serde_yaml::to_string(value).unwrap_or_else(|_| value.to_string());
    encoded
        .trim_start_matches("---\n")
        .trim_end_matches('\n')
        .to_string()
}

impl CodexSessionRecord {
    pub fn to_frontmatter(&self) -> CodexSessionFrontmatter {
        CodexSessionFrontmatter {
            id: self.id.clone(),
            session_type: CODEX_SESSION_TYPE.to_string(),
            project_id: self.project_id.clone(),
            task_id: self.task_id.clone(),
            title: self.title.clone(),
            origin: self.origin.clone(),
            status: self.status.clone(),
            cwd: self.cwd.clone(),
            codex_session_id: self.codex_session_id.clone(),
            started_at: self.started_at,
            last_seen_at: self.last_seen_at,
            ended_at: self.ended_at,
        }
    }
}

#[derive(Debug)]
struct SessionIndexEntry {
    id: String,
    thread_name: String,
    updated_at: DateTime<Utc>,
}

#[derive(Debug)]
struct SessionMeta {
    cwd: PathBuf,
    started_at: DateTime<Utc>,
}

pub fn summarize_task_for_context(title: &str, status: TaskStatus, linked: bool) -> String {
    if linked {
        format!("- [{}] {title} ({})", status.as_str(), "linked")
    } else {
        format!("- [{}] {title}", status.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
    use rusqlite::{Connection, params};
    use tempfile::TempDir;

    use super::*;

    struct ScopedEnvVar {
        key: &'static str,
        original: Option<OsString>,
        _guard: MutexGuard<'static, ()>,
    }

    impl ScopedEnvVar {
        fn set(key: &'static str, value: &Path) -> Self {
            static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
            let guard = ENV_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let original = env::var_os(key);
            unsafe {
                env::set_var(key, value);
            }
            Self {
                key,
                original,
                _guard: guard,
            }
        }
    }

    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            if let Some(original) = &self.original {
                unsafe {
                    env::set_var(self.key, original);
                }
            } else {
                unsafe {
                    env::remove_var(self.key);
                }
            }
        }
    }

    struct TestMasterPty;

    impl MasterPty for TestMasterPty {
        fn resize(&self, _size: PtySize) -> Result<(), anyhow::Error> {
            Ok(())
        }

        fn get_size(&self) -> Result<PtySize, anyhow::Error> {
            Ok(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
        }

        fn try_clone_reader(&self) -> Result<Box<dyn io::Read + Send>, anyhow::Error> {
            Ok(Box::new(io::empty()))
        }

        fn take_writer(&self) -> Result<Box<dyn io::Write + Send>, anyhow::Error> {
            Ok(Box::new(io::sink()))
        }

        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<std::os::raw::c_int> {
            None
        }

        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
            None
        }

        #[cfg(unix)]
        fn tty_name(&self) -> Option<PathBuf> {
            None
        }
    }

    #[derive(Debug, Clone, Default)]
    struct TestChild {
        kill_calls: Arc<Mutex<usize>>,
    }

    impl TestChild {
        fn kill_count(&self) -> usize {
            *self
                .kill_calls
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
        }
    }

    impl portable_pty::ChildKiller for TestChild {
        fn kill(&mut self) -> io::Result<()> {
            let mut guard = self
                .kill_calls
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *guard += 1;
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl Child for TestChild {
        fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
            Ok(portable_pty::ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            Some(42)
        }
    }

    fn test_codex_session_record(
        id: &str,
        started_at: DateTime<Utc>,
        codex_session_id: Option<&str>,
    ) -> CodexSessionRecord {
        CodexSessionRecord {
            id: id.to_string(),
            project_id: "prj_1".to_string(),
            task_id: None,
            title: "New Codex session".to_string(),
            origin: CodexSessionOrigin::Topside,
            status: CodexSessionStatus::Launching,
            cwd: "/tmp/project".to_string(),
            codex_session_id: codex_session_id.map(str::to_string),
            started_at,
            last_seen_at: started_at,
            ended_at: None,
            summary: "Summary pending.".to_string(),
            path: format!("/tmp/{id}.md"),
        }
    }

    fn test_codex_history_session(
        codex_session_id: &str,
        updated_at: DateTime<Utc>,
        cwd: &Path,
    ) -> CodexHistorySession {
        CodexHistorySession {
            codex_session_id: codex_session_id.to_string(),
            thread_name: format!("Thread {codex_session_id}"),
            cwd: cwd.to_path_buf(),
            started_at: updated_at,
            updated_at,
        }
    }

    #[test]
    fn codex_session_markdown_round_trip() -> Result<()> {
        let path = PathBuf::from("/tmp/agents/prj_1/ags_1.md");
        let frontmatter = CodexSessionFrontmatter {
            id: "ags_1".to_string(),
            session_type: CODEX_SESSION_TYPE.to_string(),
            project_id: "prj_1".to_string(),
            task_id: Some("tsk_1".to_string()),
            title: "Investigate".to_string(),
            origin: CodexSessionOrigin::Topside,
            status: CodexSessionStatus::Live,
            cwd: "/tmp/project".to_string(),
            codex_session_id: Some("019bfd79-5282-7551-95ce-cb61664a2993".to_string()),
            started_at: Utc.with_ymd_and_hms(2026, 3, 7, 12, 0, 0).unwrap(),
            last_seen_at: Utc.with_ymd_and_hms(2026, 3, 7, 12, 5, 0).unwrap(),
            ended_at: None,
        };
        let raw = render_codex_session_markdown(&frontmatter, "Summary pending.\n");
        let parsed = parse_codex_session_markdown(&raw, &path)?;
        assert_eq!(parsed.id, "ags_1");
        assert_eq!(parsed.project_id, "prj_1");
        assert_eq!(parsed.task_id.as_deref(), Some("tsk_1"));
        assert_eq!(parsed.status, CodexSessionStatus::Live);
        assert_eq!(
            parsed.codex_session_id.as_deref(),
            Some("019bfd79-5282-7551-95ce-cb61664a2993")
        );
        assert_eq!(parsed.summary.trim(), "Summary pending.");
        Ok(())
    }

    #[test]
    fn discover_codex_history_filters_by_project_root() -> Result<()> {
        let temp = TempDir::new()?;
        let codex_home = temp.path().join(".codex");
        let project_root = temp.path().join("project");
        fs::create_dir_all(&project_root)?;
        fs::create_dir_all(codex_home.join("sessions/2026/03/07"))?;
        fs::write(
            codex_home.join("session_index.jsonl"),
            r#"{"id":"019bfd79-5282-7551-95ce-cb61664a2993","thread_name":"Investigate","updated_at":"2026-03-07T16:05:00Z"}"#,
        )?;
        fs::write(
            codex_home.join("sessions/2026/03/07/rollout-2026-03-07T16-00-00-019bfd79-5282-7551-95ce-cb61664a2993.jsonl"),
            format!(
                r#"{{"timestamp":"2026-03-07T16:00:00Z","type":"session_meta","payload":{{"id":"019bfd79-5282-7551-95ce-cb61664a2993","timestamp":"2026-03-07T16:00:00Z","cwd":"{}"}}}}"#,
                project_root.to_string_lossy()
            ),
        )?;

        let _env = ScopedEnvVar::set("TOPSIDE_CODEX_HOME", &codex_home);
        let discovered = discover_codex_history_for_root(&project_root)?;

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].thread_name, "Investigate");
        assert_eq!(
            discovered[0].codex_session_id,
            "019bfd79-5282-7551-95ce-cb61664a2993"
        );
        Ok(())
    }

    #[test]
    fn discover_codex_history_falls_back_to_thread_store() -> Result<()> {
        let temp = TempDir::new()?;
        let codex_home = temp.path().join(".codex");
        let project_root = temp.path().join("project");
        fs::create_dir_all(&codex_home)?;
        fs::create_dir_all(&project_root)?;

        let connection = Connection::open(codex_home.join("state_5.sqlite"))?;
        connection.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                model_provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                title TEXT NOT NULL,
                sandbox_policy TEXT NOT NULL,
                approval_mode TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                has_user_event INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_at INTEGER,
                git_sha TEXT,
                git_branch TEXT,
                git_origin_url TEXT,
                cli_version TEXT NOT NULL DEFAULT '',
                first_user_message TEXT NOT NULL DEFAULT '',
                agent_nickname TEXT,
                agent_role TEXT,
                memory_mode TEXT NOT NULL DEFAULT 'enabled'
            );",
        )?;
        connection.execute(
            "INSERT INTO threads (
                id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
                sandbox_policy, approval_mode, tokens_used, has_user_event, archived, cli_version,
                first_user_message, memory_mode
            ) VALUES (?1, ?2, ?3, ?4, 'cli', 'openai', ?5, '', '{}', 'on-request', 0, 0, 0, '0.111.0', '', 'enabled')",
            params![
                "019bfd79-5282-7551-95ce-cb61664a2993",
                "/tmp/rollout.jsonl",
                1_762_308_000_i64,
                1_762_308_009_i64,
                project_root.to_string_lossy().to_string(),
            ],
        )?;

        let _env = ScopedEnvVar::set("TOPSIDE_CODEX_HOME", &codex_home);
        let discovered = discover_codex_history_for_root(&project_root)?;

        assert_eq!(discovered.len(), 1);
        assert_eq!(
            discovered[0].codex_session_id,
            "019bfd79-5282-7551-95ce-cb61664a2993"
        );
        assert!(discovered[0].thread_name.is_empty());
        assert_eq!(discovered[0].cwd, project_root.canonicalize()?);
        Ok(())
    }

    #[test]
    fn select_codex_launch_history_candidate_skips_claimed_threads() {
        let cwd = Path::new("/tmp/project");
        let current = test_codex_session_record(
            "ags_new",
            Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 29).unwrap(),
            None,
        );
        let existing_sessions = vec![
            test_codex_session_record(
                "ags_old",
                Utc.with_ymd_and_hms(2026, 3, 8, 16, 57, 31).unwrap(),
                Some("claimed-thread"),
            ),
            current.clone(),
        ];
        let history = vec![
            test_codex_history_session(
                "claimed-thread",
                Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 40).unwrap(),
                cwd,
            ),
            test_codex_history_session(
                "fresh-thread",
                Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 39).unwrap(),
                cwd,
            ),
        ];

        let selected =
            select_codex_launch_history_candidate(&current, cwd, &history, &existing_sessions)
                .expect("expected a fresh unclaimed history item");

        assert_eq!(selected.codex_session_id, "fresh-thread");
    }

    #[test]
    fn select_codex_launch_history_candidate_ignores_entries_before_launch() {
        let cwd = Path::new("/tmp/project");
        let current = test_codex_session_record(
            "ags_new",
            Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 29).unwrap(),
            None,
        );
        let existing_sessions = vec![current.clone()];
        let history = vec![test_codex_history_session(
            "older-thread",
            Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 26).unwrap(),
            cwd,
        )];

        assert!(
            select_codex_launch_history_candidate(&current, cwd, &history, &existing_sessions)
                .is_none()
        );
    }

    #[test]
    fn select_codex_launch_history_candidate_allows_second_precision_launch_matches() {
        let cwd = Path::new("/tmp/project");
        let current = test_codex_session_record(
            "ags_new",
            Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 29).unwrap()
                + ChronoDuration::milliseconds(500),
            None,
        );
        let existing_sessions = vec![current.clone()];
        let history = vec![test_codex_history_session(
            "fresh-thread",
            Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 29).unwrap(),
            cwd,
        )];

        let selected =
            select_codex_launch_history_candidate(&current, cwd, &history, &existing_sessions)
                .expect("expected a second-precision history item to match the launch");

        assert_eq!(selected.codex_session_id, "fresh-thread");
    }

    #[test]
    fn dedupe_codex_sessions_prefers_live_record_for_duplicate_thread() {
        let mut live = test_codex_session_record(
            "ags_live",
            Utc.with_ymd_and_hms(2026, 3, 8, 16, 57, 31).unwrap(),
            Some("shared-thread"),
        );
        live.status = CodexSessionStatus::Live;
        live.title = "Update README for Topside scope".to_string();
        live.last_seen_at = Utc.with_ymd_and_hms(2026, 3, 8, 17, 26, 20).unwrap();

        let mut duplicate = test_codex_session_record(
            "ags_dup",
            Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 29).unwrap(),
            Some("shared-thread"),
        );
        duplicate.status = CodexSessionStatus::Resumable;
        duplicate.title = "Update README for Topside scope".to_string();
        duplicate.last_seen_at = Utc.with_ymd_and_hms(2026, 3, 8, 17, 26, 57).unwrap();
        duplicate.ended_at = Some(Utc.with_ymd_and_hms(2026, 3, 8, 17, 26, 57).unwrap());

        let deduped = dedupe_codex_sessions(vec![duplicate, live.clone()]);

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].id, live.id);
    }

    #[test]
    fn keep_topside_codex_sessions_drops_discovered_records() {
        let started_at = Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 29).unwrap();
        let topside = test_codex_session_record("ags_topside", started_at, Some("thread-1"));
        let mut discovered =
            test_codex_session_record("ags_discovered", started_at, Some("thread-2"));
        discovered.origin = CodexSessionOrigin::Discovered;

        let sessions = keep_topside_codex_sessions(vec![topside.clone(), discovered]);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, topside.id);
    }

    #[test]
    fn public_codex_session_queries_exclude_discovered_records() -> Result<()> {
        let temp = TempDir::new()?;
        let config = AppConfig::default_for_workspace(temp.path().to_path_buf());
        config.ensure_workspace_dirs()?;
        let store = CodexSessionStore::new(config);
        let started_at = Utc.with_ymd_and_hms(2026, 3, 8, 17, 14, 29).unwrap();

        let topside = store.create_session(NewCodexSession {
            project_id: "prj_1".to_string(),
            task_id: None,
            title: "Topside session".to_string(),
            origin: CodexSessionOrigin::Topside,
            status: CodexSessionStatus::Live,
            cwd: "/tmp/project".to_string(),
            codex_session_id: Some("thread-topside".to_string()),
            started_at,
            last_seen_at: started_at,
            ended_at: None,
            summary: "Summary pending.".to_string(),
        })?;
        let _discovered = store.create_session(NewCodexSession {
            project_id: "prj_1".to_string(),
            task_id: None,
            title: "Discovered session".to_string(),
            origin: CodexSessionOrigin::Discovered,
            status: CodexSessionStatus::Resumable,
            cwd: "/tmp/project".to_string(),
            codex_session_id: Some("thread-discovered".to_string()),
            started_at,
            last_seen_at: started_at,
            ended_at: Some(started_at),
            summary: "Imported from Codex history.".to_string(),
        })?;

        let project_sessions = store.list_project_sessions("prj_1")?;
        let all_sessions = store.list_all_sessions()?;
        let counts = store.list_counts_by_project()?;

        assert_eq!(project_sessions.len(), 1);
        assert_eq!(project_sessions[0].id, topside.id);
        assert_eq!(all_sessions.len(), 1);
        assert_eq!(all_sessions[0].id, topside.id);
        assert_eq!(counts.get("prj_1").map(|entry| entry.total_count), Some(1));
        assert_eq!(counts.get("prj_1").map(|entry| entry.live_count), Some(1));
        Ok(())
    }

    #[test]
    fn archive_session_normalizes_launching_record_before_moving() -> Result<()> {
        let temp = TempDir::new()?;
        let workspace_root = temp.path().to_path_buf();
        let config = AppConfig::default_for_workspace(workspace_root.clone());
        let service = Arc::new(AppService::bootstrap(config.clone())?);
        let manager = CodexSessionManager::new(service)?;
        let now = Utc.with_ymd_and_hms(2026, 3, 8, 17, 44, 8).unwrap();

        let record = manager.store.create_session(NewCodexSession {
            project_id: "prj_1".to_string(),
            task_id: None,
            title: "New Codex session".to_string(),
            origin: CodexSessionOrigin::Topside,
            status: CodexSessionStatus::Launching,
            cwd: "/tmp/project".to_string(),
            codex_session_id: None,
            started_at: now,
            last_seen_at: now,
            ended_at: None,
            summary: build_summary_stub(None, None, now, "Launching"),
        })?;

        manager.archive_session(&record.id)?;

        assert!(manager.store.get_session(&record.id)?.is_none());
        let archived_path = config
            .archive_dir()
            .join("codex_sessions")
            .join("prj_1")
            .join(format!("{}.md", record.id));
        assert!(archived_path.exists());

        let archived = manager.store.read_record(&archived_path)?;
        assert_eq!(archived.status, CodexSessionStatus::Resumable);
        assert!(archived.ended_at.is_some());
        assert!(archived.last_seen_at >= now);
        Ok(())
    }

    #[test]
    fn archive_session_detaches_live_runtime_before_process_exit() -> Result<()> {
        let temp = TempDir::new()?;
        let workspace_root = temp.path().to_path_buf();
        let config = AppConfig::default_for_workspace(workspace_root.clone());
        let service = Arc::new(AppService::bootstrap(config.clone())?);
        let manager = CodexSessionManager::new(service)?;
        let now = Utc.with_ymd_and_hms(2026, 3, 8, 18, 2, 41).unwrap();

        let record = manager.store.create_session(NewCodexSession {
            project_id: "prj_1".to_string(),
            task_id: None,
            title: "Live Codex session".to_string(),
            origin: CodexSessionOrigin::Topside,
            status: CodexSessionStatus::Live,
            cwd: "/tmp/project".to_string(),
            codex_session_id: Some("thread-live".to_string()),
            started_at: now,
            last_seen_at: now,
            ended_at: None,
            summary: build_summary_stub(Some("Live Codex session"), None, now, "Live"),
        })?;

        let child = TestChild::default();
        let runtime = Arc::new(LiveCodexSession::new(
            Box::new(TestMasterPty),
            Box::new(io::sink()),
            Box::new(child.clone()),
        ));
        manager
            .runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(record.id.clone(), runtime);

        manager.archive_session(&record.id)?;

        assert!(!manager.is_live(&record.id));
        assert!(manager.subscribe(&record.id).is_err());
        assert_eq!(child.kill_count(), 1);

        let archived_path = config
            .archive_dir()
            .join("codex_sessions")
            .join("prj_1")
            .join(format!("{}.md", record.id));
        assert!(archived_path.exists());
        Ok(())
    }

    #[test]
    fn attach_discovered_session_id_recovers_missing_thread_id_from_history() -> Result<()> {
        let temp = TempDir::new()?;
        let workspace_root = temp.path().to_path_buf();
        let config = AppConfig::default_for_workspace(workspace_root.clone());
        let service = Arc::new(AppService::bootstrap(config)?);
        let manager = CodexSessionManager::new(service)?;

        let project_root = temp.path().join("project");
        fs::create_dir_all(&project_root)?;

        let codex_home = temp.path().join(".codex");
        fs::create_dir_all(codex_home.join("sessions/2026/03/09"))?;
        fs::write(
            codex_home.join("session_index.jsonl"),
            r#"{"id":"019bfd79-5282-7551-95ce-cb61664a2993","thread_name":"Recovered Session","updated_at":"2026-03-09T01:06:02Z"}"#,
        )?;
        fs::write(
            codex_home.join("sessions/2026/03/09/rollout-2026-03-09T01-06-02-019bfd79-5282-7551-95ce-cb61664a2993.jsonl"),
            format!(
                r#"{{"timestamp":"2026-03-09T01:06:02Z","type":"session_meta","payload":{{"id":"019bfd79-5282-7551-95ce-cb61664a2993","timestamp":"2026-03-09T01:06:02Z","cwd":"{}"}}}}"#,
                project_root.to_string_lossy()
            ),
        )?;

        let _home_env = ScopedEnvVar::set("TOPSIDE_CODEX_HOME", &codex_home);

        let started_at = Utc.with_ymd_and_hms(2026, 3, 9, 1, 6, 0).unwrap();
        let record = manager.store.create_session(NewCodexSession {
            project_id: "prj_1".to_string(),
            task_id: None,
            title: "New Codex session".to_string(),
            origin: CodexSessionOrigin::Topside,
            status: CodexSessionStatus::Resumable,
            cwd: project_root.to_string_lossy().to_string(),
            codex_session_id: None,
            started_at,
            last_seen_at: started_at,
            ended_at: Some(started_at),
            summary: build_summary_stub(None, None, started_at, "Resumable"),
        })?;

        let recovered = manager.attach_discovered_session_id(&record)?;

        assert_eq!(recovered.status, CodexSessionStatus::Resumable);
        assert_eq!(
            recovered.codex_session_id.as_deref(),
            Some("019bfd79-5282-7551-95ce-cb61664a2993")
        );
        assert_eq!(recovered.title, "Recovered Session");
        Ok(())
    }

    #[test]
    fn attach_discovered_session_id_recovers_missing_thread_id_from_thread_store() -> Result<()> {
        let temp = TempDir::new()?;
        let workspace_root = temp.path().to_path_buf();
        let config = AppConfig::default_for_workspace(workspace_root.clone());
        let service = Arc::new(AppService::bootstrap(config)?);
        let manager = CodexSessionManager::new(service)?;

        let project_root = temp.path().join("project");
        fs::create_dir_all(&project_root)?;

        let codex_home = temp.path().join(".codex");
        fs::create_dir_all(&codex_home)?;
        let connection = Connection::open(codex_home.join("state_5.sqlite"))?;
        let started_at = Utc.with_ymd_and_hms(2025, 11, 6, 0, 26, 38).unwrap();
        connection.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                model_provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                title TEXT NOT NULL,
                sandbox_policy TEXT NOT NULL,
                approval_mode TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                has_user_event INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_at INTEGER,
                git_sha TEXT,
                git_branch TEXT,
                git_origin_url TEXT,
                cli_version TEXT NOT NULL DEFAULT '',
                first_user_message TEXT NOT NULL DEFAULT '',
                agent_nickname TEXT,
                agent_role TEXT,
                memory_mode TEXT NOT NULL DEFAULT 'enabled'
            );",
        )?;
        connection.execute(
            "INSERT INTO threads (
                id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
                sandbox_policy, approval_mode, tokens_used, has_user_event, archived, cli_version,
                first_user_message, memory_mode
            ) VALUES (?1, ?2, ?3, ?4, 'cli', 'openai', ?5, '', '{}', 'on-request', 0, 0, 0, '0.111.0', '', 'enabled')",
            params![
                "019bfd79-5282-7551-95ce-cb61664a2993",
                "/tmp/rollout.jsonl",
                started_at.timestamp(),
                started_at.timestamp() + 2,
                project_root.to_string_lossy().to_string(),
            ],
        )?;

        let _home_env = ScopedEnvVar::set("TOPSIDE_CODEX_HOME", &codex_home);
        let record = manager.store.create_session(NewCodexSession {
            project_id: "prj_1".to_string(),
            task_id: None,
            title: "New Codex session".to_string(),
            origin: CodexSessionOrigin::Topside,
            status: CodexSessionStatus::Resumable,
            cwd: project_root.to_string_lossy().to_string(),
            codex_session_id: None,
            started_at,
            last_seen_at: started_at,
            ended_at: Some(started_at),
            summary: build_summary_stub(None, None, started_at, "Resumable"),
        })?;

        let recovered = manager.attach_discovered_session_id(&record)?;

        assert_eq!(
            recovered.codex_session_id.as_deref(),
            Some("019bfd79-5282-7551-95ce-cb61664a2993")
        );
        assert_eq!(recovered.title, "New Codex session");
        Ok(())
    }

    #[test]
    fn build_codex_command_uses_supported_interactive_flags() {
        let project_root = PathBuf::from("/tmp/project");
        let workspace_root = PathBuf::from("/tmp/workspace");
        let command = build_codex_command(
            "codex",
            &project_root,
            &workspace_root,
            Some("Investigate the failing session"),
            Some("019bfd79-5282-7551-95ce-cb61664a2993"),
        );

        let argv = command
            .get_argv()
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(argv[0], "codex");
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["resume", "019bfd79-5282-7551-95ce-cb61664a2993"])
        );
        assert!(argv.contains(&"-C".to_string()));
        assert!(
            argv.iter()
                .any(|value| value == "check_for_update_on_startup=false")
        );
        assert!(
            argv.iter()
                .any(|value| value == "Investigate the failing session")
        );
        assert!(!argv.iter().any(|value| value == "--skip-git-repo-check"));
        assert_eq!(
            command
                .get_env("TERM")
                .map(|value| value.to_string_lossy().to_string()),
            Some("xterm-256color".to_string())
        );
        assert_eq!(
            command
                .get_env("COLORTERM")
                .map(|value| value.to_string_lossy().to_string()),
            Some("truecolor".to_string())
        );
        assert_eq!(
            command
                .get_env("TERM_PROGRAM")
                .map(|value| value.to_string_lossy().to_string()),
            Some("Topside".to_string())
        );
    }

    #[test]
    fn build_codex_command_disables_user_mcp_servers() -> Result<()> {
        let temp = TempDir::new()?;
        let codex_home = temp.path().join(".codex");
        fs::create_dir_all(&codex_home)?;
        fs::write(
            codex_home.join("config.toml"),
            r#"
[mcp_servers.n10e]
command = "n10e"

[mcp_servers.other]
command = "other"
"#,
        )?;

        let _env = ScopedEnvVar::set("TOPSIDE_CODEX_HOME", &codex_home);

        let command = build_codex_command(
            "codex",
            Path::new("/tmp/project"),
            Path::new("/tmp/workspace"),
            None,
            None,
        );

        let argv = command
            .get_argv()
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(
            argv.iter()
                .any(|value| value == "mcp_servers.n10e.enabled=false")
        );
        assert!(
            argv.iter()
                .any(|value| value == "mcp_servers.other.enabled=false")
        );
        assert!(
            argv.iter()
                .any(|value| value.contains("mcp_servers.topside.command"))
        );
        Ok(())
    }

    #[test]
    fn parse_codex_rollout_transcript_prefers_event_messages() -> Result<()> {
        let temp = TempDir::new()?;
        let rollout = temp.path().join("rollout.jsonl");
        fs::write(
            &rollout,
            concat!(
                "{\"timestamp\":\"2026-03-10T14:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-1\",\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-03-10T14:00:00Z\"}}\n",
                "{\"timestamp\":\"2026-03-10T14:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Execute the following task: Fix the thing\"}}\n",
                "{\"timestamp\":\"2026-03-10T14:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"I'm on it.\"}}\n",
                "{\"timestamp\":\"2026-03-10T14:00:03Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"# AGENTS.md instructions for /tmp/project\"}]}}\n"
            ),
        )?;

        let transcript = parse_codex_rollout_transcript(&rollout)?;

        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].role, CodexTranscriptRole::User);
        assert_eq!(
            transcript[0].text,
            "Execute the following task: Fix the thing"
        );
        assert_eq!(transcript[1].role, CodexTranscriptRole::Assistant);
        assert_eq!(transcript[1].text, "I'm on it.");
        Ok(())
    }

    #[test]
    fn parse_codex_rollout_transcript_falls_back_to_response_messages() -> Result<()> {
        let temp = TempDir::new()?;
        let rollout = temp.path().join("rollout.jsonl");
        fs::write(
            &rollout,
            concat!(
                "{\"timestamp\":\"2026-03-10T14:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-1\",\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-03-10T14:00:00Z\"}}\n",
                "{\"timestamp\":\"2026-03-10T14:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"# AGENTS.md instructions for /tmp/project\"}]}}\n",
                "{\"timestamp\":\"2026-03-10T14:00:02Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Investigate the failing build\"}]}}\n",
                "{\"timestamp\":\"2026-03-10T14:00:03Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"I found the issue.\"}]}}\n"
            ),
        )?;

        let transcript = parse_codex_rollout_transcript(&rollout)?;

        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].role, CodexTranscriptRole::User);
        assert_eq!(transcript[0].text, "Investigate the failing build");
        assert_eq!(transcript[1].role, CodexTranscriptRole::Assistant);
        assert_eq!(transcript[1].text, "I found the issue.");
        Ok(())
    }

    #[test]
    fn terminal_backlog_retains_recent_output_within_byte_limit() {
        let chunk = "x".repeat((OUTPUT_BACKLOG_BYTES / 2).max(1));
        let mut backlog = TerminalBacklog::default();

        backlog.push("first".to_string());
        backlog.push(chunk.clone());
        backlog.push(chunk.clone());
        backlog.push("tail".to_string());

        let replay = backlog.to_vec();
        assert!(!replay.iter().any(|item| item == "first"));
        assert_eq!(replay.last().map(String::as_str), Some("tail"));
        let replay_bytes = replay.iter().map(String::len).sum::<usize>();
        assert!(replay_bytes <= OUTPUT_BACKLOG_BYTES + "tail".len());
    }
}
