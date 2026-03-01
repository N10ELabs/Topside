use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::constants::{APP_DIR, CONFIG_FILE_NAME, INDEX_DB_NAME, PROJECT_CODENAME};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub codename: String,
    pub workspace_root: PathBuf,
    pub dirs: DirConfig,
    pub server: ServerConfig,
    pub index: IndexConfig,
    pub search: SearchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirConfig {
    pub projects: String,
    pub tasks: String,
    pub notes: String,
    pub agents: String,
    pub archive: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    pub debounce_ms: u64,
    pub startup_full_scan: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    pub default_limit: usize,
    pub bm25_k1: f32,
    pub bm25_b: f32,
}

impl AppConfig {
    pub fn default_for_workspace(workspace_root: PathBuf) -> Self {
        Self {
            codename: PROJECT_CODENAME.to_string(),
            workspace_root,
            dirs: DirConfig::default(),
            server: ServerConfig::default(),
            index: IndexConfig::default(),
            search: SearchConfig::default(),
        }
    }

    pub fn load_from_workspace(workspace_root: &Path) -> Result<Self> {
        let path = workspace_root.join(CONFIG_FILE_NAME);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed reading config at {}", path.display()))?;
        let mut cfg: AppConfig = toml::from_str(&raw)
            .with_context(|| format!("failed parsing config at {}", path.display()))?;
        if cfg.workspace_root.as_os_str().is_empty() {
            cfg.workspace_root = workspace_root.to_path_buf();
        }
        if cfg.workspace_root.is_relative() {
            cfg.workspace_root = workspace_root.join(&cfg.workspace_root);
        }
        Ok(cfg)
    }

    pub fn save_to_workspace(&self, workspace_root: &Path) -> Result<PathBuf> {
        let cfg_path = workspace_root.join(CONFIG_FILE_NAME);
        let raw = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(&cfg_path, raw)
            .with_context(|| format!("failed writing config at {}", cfg_path.display()))?;
        Ok(cfg_path)
    }

    pub fn ensure_workspace_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.workspace_root.join(APP_DIR)).with_context(|| {
            format!(
                "failed creating app directory at {}",
                self.workspace_root.join(APP_DIR).display()
            )
        })?;

        for dir in [
            self.projects_dir(),
            self.tasks_dir(),
            self.notes_dir(),
            self.agents_dir(),
            self.archive_dir(),
        ] {
            fs::create_dir_all(&dir).with_context(|| {
                format!("failed creating workspace directory {}", dir.display())
            })?;
        }

        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.workspace_root.join(APP_DIR).join(INDEX_DB_NAME)
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.workspace_root.join(&self.dirs.projects)
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.workspace_root.join(&self.dirs.tasks)
    }

    pub fn notes_dir(&self) -> PathBuf {
        self.workspace_root.join(&self.dirs.notes)
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.workspace_root.join(&self.dirs.agents)
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.workspace_root.join(&self.dirs.archive)
    }

    pub fn canonical_workspace_root(&self) -> Result<PathBuf> {
        self.workspace_root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", self.workspace_root.display()))
    }

    pub fn resolve_relative_path(&self, relative: &Path) -> PathBuf {
        self.workspace_root.join(relative)
    }

    pub fn ensure_within_workspace(&self, path: &Path) -> Result<()> {
        let ws = self.canonical_workspace_root()?;
        let parent = path
            .parent()
            .with_context(|| format!("path has no parent: {}", path.display()))?;
        let canonical_parent = if parent.exists() {
            parent
                .canonicalize()
                .with_context(|| format!("failed canonicalizing {}", parent.display()))?
        } else {
            let mut probe = parent.to_path_buf();
            while !probe.exists() {
                probe = probe
                    .parent()
                    .map(Path::to_path_buf)
                    .context("no existing parent found while validating workspace boundary")?;
            }
            probe
                .canonicalize()
                .with_context(|| format!("failed canonicalizing {}", probe.display()))?
        };

        if !canonical_parent.starts_with(&ws) {
            anyhow::bail!(
                "path {} is outside workspace root {}",
                path.display(),
                ws.display()
            );
        }

        Ok(())
    }
}

impl Default for DirConfig {
    fn default() -> Self {
        Self {
            projects: "projects".to_string(),
            tasks: "tasks".to_string(),
            notes: "notes".to_string(),
            agents: "agents".to_string(),
            archive: "archive".to_string(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 7410,
        }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 350,
            startup_full_scan: true,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_limit: 20,
            bm25_k1: 1.2,
            bm25_b: 0.75,
        }
    }
}
