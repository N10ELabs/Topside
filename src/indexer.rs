use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{error, warn};
use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::db::Db;
use crate::markdown::parse_entity_markdown;
use crate::types::IndexedEntity;

#[derive(Clone)]
pub struct Indexer {
    pub config: AppConfig,
    pub db: Db,
}

pub struct WatcherRuntime {
    _watcher: RecommendedWatcher,
    _thread: std::thread::JoinHandle<()>,
}

impl Indexer {
    pub fn new(config: AppConfig, db: Db) -> Self {
        Self { config, db }
    }

    pub fn full_scan(&self) -> Result<()> {
        let mut discovered = HashSet::new();

        for entry in WalkDir::new(&self.config.workspace_root)
            .into_iter()
            .filter_entry(|entry| !is_ignored(entry.path()))
        {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path().to_path_buf();
            if !is_markdown(&path) {
                continue;
            }

            discovered.insert(path.to_string_lossy().to_string());
            if let Err(err) = self.index_file(&path) {
                warn!(error = %err, path = %path.display(), "failed indexing markdown file");
            }
        }

        for known in self.db.list_indexed_paths()? {
            if !discovered.contains(&known) {
                let path = PathBuf::from(known);
                if let Err(err) = self.db.remove_by_path(&path) {
                    warn!(error = %err, path = %path.display(), "failed cleaning stale indexed path");
                }
            }
        }

        Ok(())
    }

    pub fn index_file(&self, path: &Path) -> Result<IndexedEntity> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed reading markdown file {}", path.display()))?;
        let parsed = parse_entity_markdown(&raw)
            .with_context(|| format!("failed parsing markdown file {}", path.display()))?;

        let tags = parsed
            .frontmatter
            .tags()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|v| !v.trim().is_empty())
            .collect::<Vec<_>>();

        let indexed = IndexedEntity {
            id: parsed.frontmatter.id().to_string(),
            entity_type: parsed.frontmatter.entity_type(),
            title: parsed.frontmatter.title().to_string(),
            path: path.to_path_buf(),
            body: parsed.body,
            project_id: parsed.frontmatter.project_id().map(ToString::to_string),
            status: parsed.frontmatter.status(),
            priority: parsed.frontmatter.priority(),
            assignee: parsed.frontmatter.assignee().map(ToString::to_string),
            due_at: parsed.frontmatter.due_at(),
            sort_order: parsed.frontmatter.sort_order().unwrap_or_default(),
            completed_at: parsed.frontmatter.completed_at(),
            owner: parsed.frontmatter.owner().map(ToString::to_string),
            source_kind: parsed.frontmatter.source_kind(),
            source_locator: parsed.frontmatter.source_locator().map(ToString::to_string),
            tags,
            created_at: parsed.frontmatter.created_at(),
            updated_at: parsed.frontmatter.updated_at(),
            revision: parsed.revision,
            archived: path.starts_with(self.config.archive_dir()),
            links: parsed.links,
        };

        self.db.upsert_indexed_entity(&indexed)?;
        Ok(indexed)
    }

    pub fn remove_path(&self, path: &Path) -> Result<()> {
        self.db.remove_by_path(path)
    }

    pub fn start_watcher(self: Arc<Self>) -> Result<WatcherRuntime> {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })?;

        watcher.watch(&self.config.workspace_root, RecursiveMode::Recursive)?;

        let debounce = Duration::from_millis(self.config.index.debounce_ms.max(50));
        let indexer = Arc::clone(&self);

        let thread = std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut paths: HashMap<PathBuf, bool> = HashMap::new();
                let mut needs_full_rescan = collect_paths(&mut paths, first);

                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(next) => {
                            needs_full_rescan = needs_full_rescan || collect_paths(&mut paths, next)
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }

                if needs_full_rescan {
                    if let Err(err) = indexer.full_scan() {
                        warn!(error = %err, "failed performing full rescan after watcher overflow");
                    }
                    continue;
                }

                for (path, deleted) in paths {
                    if !is_markdown(&path) {
                        continue;
                    }
                    if is_ignored(&path) {
                        continue;
                    }

                    if deleted || !path.exists() {
                        if let Err(err) = indexer.remove_path(&path) {
                            warn!(error = %err, path = %path.display(), "failed removing deleted path from index");
                        }
                    } else if let Err(err) = indexer.index_file(&path) {
                        warn!(error = %err, path = %path.display(), "failed indexing changed path");
                    }
                }
            }
        });

        Ok(WatcherRuntime {
            _watcher: watcher,
            _thread: thread,
        })
    }

    pub fn import_tree(&self, source: &Path) -> Result<usize> {
        if !source.exists() {
            anyhow::bail!("import source does not exist: {}", source.display());
        }

        let import_root = self.config.notes_dir().join("imported");
        std::fs::create_dir_all(&import_root)
            .with_context(|| format!("failed creating {}", import_root.display()))?;

        let mut imported = 0usize;
        for entry in WalkDir::new(source) {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !is_markdown(path) {
                continue;
            }

            let relative = path.strip_prefix(source).unwrap_or(path).to_path_buf();
            let target = import_root.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(path, &target).with_context(|| {
                format!(
                    "failed copying import file from {} to {}",
                    path.display(),
                    target.display()
                )
            })?;
            imported += 1;
            if let Err(err) = self.index_file(&target) {
                warn!(error = %err, path = %target.display(), "failed indexing imported markdown file");
            }
        }

        Ok(imported)
    }
}

fn collect_paths(paths: &mut HashMap<PathBuf, bool>, event: notify::Result<Event>) -> bool {
    match event {
        Ok(event) => {
            let deleted = matches!(event.kind, EventKind::Remove(_));
            let is_overflow = event.need_rescan();

            if is_overflow {
                error!("file watcher overflow detected; triggering full rescan");
                return true;
            }

            for path in event.paths {
                paths
                    .entry(path)
                    .and_modify(|value| *value = *value || deleted)
                    .or_insert(deleted);
            }
            false
        }
        Err(err) => {
            warn!(error = %err, "file watcher emitted error event");
            false
        }
    }
}

fn is_ignored(path: &Path) -> bool {
    path.components().any(|part| {
        let p = part.as_os_str().to_string_lossy();
        p == ".git" || p == ".n10e"
    })
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}
