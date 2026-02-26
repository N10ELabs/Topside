use std::path::Path;

use anyhow::Result;
use tempfile::TempDir;

use n10e::config::AppConfig;
use n10e::service::AppService;

pub fn setup_service_workspace() -> Result<(TempDir, AppService)> {
    let tmp = TempDir::new()?;
    let root = tmp.path().to_path_buf();
    let config = prepare_workspace_config(&root)?;
    let service = AppService::bootstrap(config)?;
    Ok((tmp, service))
}

pub fn prepare_workspace_config(workspace_root: &Path) -> Result<AppConfig> {
    let config = AppConfig::default_for_workspace(workspace_root.to_path_buf());
    config.ensure_workspace_dirs()?;
    config.save_to_workspace(workspace_root)?;
    Ok(config)
}
