use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::warn;
use ulid::Ulid;
use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::markdown::split_frontmatter;
use crate::service::{AppService, ServiceError};
use crate::types::TaskStatus;

const CODEX_SESSION_TYPE: &str = "codex_session";
const OUTPUT_BACKLOG_CHUNKS: usize = 256;
const DEFAULT_PTY_ROWS: u16 = 30;
const DEFAULT_PTY_COLS: u16 = 110;
const CODEX_RECONCILE_TIMEOUT: Duration = Duration::from_secs(12);
const CODEX_RECONCILE_POLL_INTERVAL: Duration = Duration::from_millis(600);
const CODEX_ARCHIVE_TIMEOUT: Duration = Duration::from_secs(4);
const CODEX_ARCHIVE_POLL_INTERVAL: Duration = Duration::from_millis(60);

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
        let dir = self.project_dir(project_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed reading {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            sessions.push(self.read_record(&path)?);
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    pub fn list_all_sessions(&self) -> Result<Vec<CodexSessionRecord>> {
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
        sort_sessions(&mut sessions);
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
        for session in self.list_all_sessions()? {
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
        for session in self.list_all_sessions()? {
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
        self.project_dir(project_id).join(format!("{session_id}.md"))
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

    pub fn create_project_session(
        &self,
        project_id: &str,
        task_id: Option<String>,
    ) -> Result<CodexSessionRecord, ServiceError> {
        let cwd = self.service.local_project_source_root(project_id)?;
        let title = self.service.suggest_codex_session_title(project_id, task_id.as_deref())?;
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

        self.spawn_runtime(&record, None, None, false)?;
        self.spawn_codex_session_reconciler(record.id.clone(), cwd);
        self.store
            .get_session(&record.id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("codex session {} missing after launch", record.id))
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
        let codex_session_id = record
            .codex_session_id
            .clone()
            .with_context(|| format!("codex session {} cannot be resumed yet", record.id))
            .map_err(ServiceError::Other)?;
        self.spawn_runtime(
            &record,
            None,
            Some(codex_session_id.as_str()),
            true,
        )?;
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
            .map_err(ServiceError::Other)
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
        runtime
            .push_message(CodexTerminalServerMessage::Status {
                status: CodexSessionStatus::Resumable.as_str().to_string(),
            });
        runtime.terminate().map_err(ServiceError::Other)
    }

    pub fn archive_session(&self, session_id: &str) -> Result<(), ServiceError> {
        self.store
            .get_session(session_id)
            .map_err(ServiceError::Other)?
            .with_context(|| format!("codex session {session_id} not found"))
            .map_err(ServiceError::Other)?;

        if self.is_live(session_id) {
            self.terminate_session(session_id)?;
            let started = Instant::now();
            while self.is_live(session_id) && started.elapsed() < CODEX_ARCHIVE_TIMEOUT {
                std::thread::sleep(CODEX_ARCHIVE_POLL_INTERVAL);
            }
            if self.is_live(session_id) {
                return Err(ServiceError::Other(anyhow::anyhow!(
                    "timed out waiting for codex session {session_id} to exit before archiving"
                )));
            }
        }

        self.store
            .archive_session(session_id)
            .map_err(ServiceError::Other)
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
        let command = build_codex_command(
            &codex_binary(),
            &cwd,
            &self.service.config.workspace_root,
            prompt.as_deref(),
            resume_session_id,
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
        std::thread::spawn(move || {
            let wait_result = runtime.wait();
            let ended_at = Utc::now();
            {
                let mut guard = runtimes.lock().unwrap_or_else(|poison| poison.into_inner());
                guard.remove(&session_id);
            }
            if let Err(error) = wait_result {
                runtime.push_message(CodexTerminalServerMessage::Error {
                    message: format!("codex session exited with an error: {error}"),
                });
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
                let Ok(history) = discover_codex_history_for_root(&canonical_cwd) else {
                    continue;
                };
                let Some(found) = history
                    .into_iter()
                    .find(|item| item.cwd == canonical_cwd)
                else {
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
                let title = if current.title == "New Codex session" && !found.thread_name.trim().is_empty() {
                    Some(found.thread_name.clone())
                } else {
                    None
                };
                if let Err(error) = store.update_session(
                    &session_id,
                    CodexSessionPatch {
                        title,
                        codex_session_id: Some(Some(found.codex_session_id)),
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

struct LiveCodexSession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn Child + Send>>>,
    tx: broadcast::Sender<CodexTerminalServerMessage>,
    backlog: Arc<Mutex<VecDeque<String>>>,
}

impl LiveCodexSession {
    fn new(
        master: Box<dyn MasterPty + Send>,
        writer: Box<dyn Write + Send>,
        child: Box<dyn Child + Send>,
    ) -> Self {
        let (tx, _) = broadcast::channel(512);
        Self {
            master: Arc::new(Mutex::new(master)),
            writer: Arc::new(Mutex::new(writer)),
            child: Arc::new(Mutex::new(child)),
            tx,
            backlog: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<CodexTerminalServerMessage> {
        self.tx.subscribe()
    }

    fn backlog(&self) -> Vec<String> {
        self.backlog
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn push_output(&self, data: String) {
        {
            let mut backlog = self.backlog.lock().unwrap_or_else(|poison| poison.into_inner());
            backlog.push_back(data.clone());
            while backlog.len() > OUTPUT_BACKLOG_CHUNKS {
                backlog.pop_front();
            }
        }
        let _ = self.tx.send(CodexTerminalServerMessage::Output { data });
    }

    fn push_message(&self, message: CodexTerminalServerMessage) {
        let _ = self.tx.send(message);
    }

    fn write_input(&self, data: &str) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|poison| poison.into_inner());
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
        self.child
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

pub fn parse_codex_session_markdown(content: &str, path: &Path) -> Result<CodexSessionRecord> {
    let (frontmatter_raw, body) = split_frontmatter(content)?;
    let frontmatter: CodexSessionFrontmatter =
        serde_yaml::from_str(&frontmatter_raw).context("failed parsing codex session frontmatter")?;
    if frontmatter.session_type != CODEX_SESSION_TYPE {
        anyhow::bail!("unsupported codex session type: {}", frontmatter.session_type);
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
    yaml.push_str(&format!("project_id: {}\n", yaml_scalar(&frontmatter.project_id)));
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
    yaml.push_str(&format!("started_at: {}\n", frontmatter.started_at.to_rfc3339()));
    yaml.push_str(&format!("last_seen_at: {}\n", frontmatter.last_seen_at.to_rfc3339()));
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

pub fn discover_codex_history_for_root(project_root: &Path) -> Result<Vec<CodexHistorySession>> {
    let canonical_root = project_root
        .canonicalize()
        .with_context(|| format!("failed canonicalizing {}", project_root.display()))?;
    let codex_home = codex_home_dir()?;
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
        if canonical_cwd == canonical_root || canonical_cwd.starts_with(&canonical_root) {
            discovered.push(CodexHistorySession {
                codex_session_id: index_entry.id,
                thread_name: index_entry.thread_name,
                cwd: canonical_cwd,
                started_at: meta.started_at,
                updated_at: index_entry.updated_at,
            });
        }
    }

    discovered.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
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
    append_codex_args(
        &mut command,
        cwd,
        workspace_root,
        prompt,
        resume_session_id,
    );
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
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        value.to_string()
    } else {
        toml_basic_string(value)
    }
}

fn toml_basic_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
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
        return Ok(Some(SessionMeta {
            cwd,
            started_at,
        }));
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

fn sort_sessions(sessions: &mut [CodexSessionRecord]) {
    sessions.sort_by(|left, right| {
        session_sort_weight(&left.status)
            .cmp(&session_sort_weight(&right.status))
            .then(right.last_seen_at.cmp(&left.last_seen_at))
            .then(left.title.cmp(&right.title))
    });
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

pub fn summarize_task_for_context(
    title: &str,
    status: TaskStatus,
    linked: bool,
) -> String {
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
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use chrono::{TimeZone, Utc};
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
        assert_eq!(parsed.codex_session_id.as_deref(), Some("019bfd79-5282-7551-95ce-cb61664a2993"));
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
        assert!(argv.windows(2).any(|pair| pair == ["resume", "019bfd79-5282-7551-95ce-cb61664a2993"]));
        assert!(argv.contains(&"-C".to_string()));
        assert!(!argv.iter().any(|value| value == "--skip-git-repo-check"));
        assert_eq!(
            command.get_env("TERM").map(|value| value.to_string_lossy().to_string()),
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
        assert!(argv.iter().any(|value| value == "mcp_servers.n10e.enabled=false"));
        assert!(argv.iter().any(|value| value == "mcp_servers.other.enabled=false"));
        assert!(argv.iter().any(|value| value.contains("mcp_servers.topside.command")));
        Ok(())
    }
}
