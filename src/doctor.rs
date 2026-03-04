use std::path::Path;

use anyhow::{Context, Result};
use notify::Watcher;

use crate::config::AppConfig;
use crate::constants::APP_DIR;
use crate::db::Db;

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub config_ok: bool,
    pub workspace_ok: bool,
    pub db_ok: bool,
    pub watcher_ok: bool,
    pub write_boundary_ok: bool,
}

impl DoctorReport {
    pub fn healthy(&self) -> bool {
        self.config_ok
            && self.workspace_ok
            && self.db_ok
            && self.watcher_ok
            && self.write_boundary_ok
    }
}

pub fn run_doctor(config: &AppConfig) -> Result<DoctorReport> {
    let config_ok = check_config(config)?;
    let workspace_ok = check_workspace(config)?;
    let db_ok = check_db(config)?;
    let watcher_ok = check_watcher(config)?;
    let write_boundary_ok = check_write_boundary(config)?;

    Ok(DoctorReport {
        config_ok,
        workspace_ok,
        db_ok,
        watcher_ok,
        write_boundary_ok,
    })
}

fn check_config(config: &AppConfig) -> Result<bool> {
    if config.codename.trim().is_empty() {
        anyhow::bail!("codename cannot be empty");
    }
    Ok(true)
}

fn check_workspace(config: &AppConfig) -> Result<bool> {
    if !config.workspace_root.exists() {
        anyhow::bail!(
            "workspace root does not exist: {}",
            config.workspace_root.display()
        );
    }

    config.ensure_workspace_dirs()?;
    Ok(true)
}

fn check_db(config: &AppConfig) -> Result<bool> {
    let db = Db::open(&config.db_path())?;
    db.run_migrations()?;
    Ok(true)
}

fn check_watcher(config: &AppConfig) -> Result<bool> {
    let mut watcher =
        notify::recommended_watcher(|_event| {}).context("failed creating filesystem watcher")?;
    watcher
        .watch(&config.workspace_root, notify::RecursiveMode::NonRecursive)
        .context("failed watching workspace root")?;
    Ok(true)
}

fn check_write_boundary(config: &AppConfig) -> Result<bool> {
    let probe_dir = config.workspace_root.join(APP_DIR);
    std::fs::create_dir_all(&probe_dir)?;
    let probe_file = probe_dir.join("doctor-probe.tmp");
    write_probe_file(&probe_file)?;
    std::fs::remove_file(&probe_file)
        .with_context(|| format!("failed removing probe file {}", probe_file.display()))?;
    Ok(true)
}

fn write_probe_file(path: &Path) -> Result<()> {
    std::fs::write(path, b"ok")
        .with_context(|| format!("failed writing probe file {}", path.display()))
}
