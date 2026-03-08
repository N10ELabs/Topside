use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use topside::config::{AppConfig, maybe_migrate_workspace_identity};
use topside::constants::PROJECT_CODENAME;
use topside::service::AppService;
use topside::types::{Actor, CreateProjectPayload, ProjectSourceKind};

#[derive(Debug, Parser)]
#[command(name = "topside-smoke-codex-setup")]
#[command(about = "Seed a disposable Topside workspace for Codex session smoke tests")]
struct Cli {
    #[arg(long, value_name = "DIR")]
    workspace: PathBuf,

    #[arg(long, value_name = "DIR")]
    repo: PathBuf,

    #[arg(long, default_value = "Codex Session Smoke")]
    title: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let workspace = cli.workspace;
    let repo = cli.repo;

    seed_repo(&repo)?;
    let config = ensure_workspace(&workspace)?;
    let service = AppService::bootstrap(config)?;

    let project = service.create_project(
        CreateProjectPayload {
            title: cli.title.clone(),
            owner: None,
            source_kind: Some(ProjectSourceKind::Local),
            source_locator: Some(
                repo.canonicalize()
                    .with_context(|| format!("failed canonicalizing {}", repo.display()))?
                    .to_string_lossy()
                    .to_string(),
            ),
            icon: Some("terminal".to_string()),
            tags: None,
            body: Some(
                "Disposable local project used for repeated Codex session smoke checks."
                    .to_string(),
            ),
        },
        Actor::human("smoke"),
    )?;

    service.ensure_local_project_user_files(&project.id, Actor::human("smoke"))?;
    let (_workspace, task_id) = service.create_task_after(
        &project.id,
        "Run Codex session lifecycle smoke checks".to_string(),
        None,
        Actor::human("smoke"),
    )?;

    println!("workspace={}", workspace.display());
    println!("repo={}", repo.display());
    println!("project_id={}", project.id);
    println!("task_id={task_id}");

    Ok(())
}

fn ensure_workspace(workspace: &Path) -> Result<AppConfig> {
    fs::create_dir_all(workspace)
        .with_context(|| format!("failed creating workspace {}", workspace.display()))?;
    maybe_migrate_workspace_identity(workspace)?;

    let config_path = workspace.join("topside.toml");
    let config = if config_path.exists() {
        let mut existing = AppConfig::load_from_workspace(workspace)?;
        existing.workspace_root = workspace.to_path_buf();
        existing
    } else {
        let mut fresh = AppConfig::default_for_workspace(workspace.to_path_buf());
        fresh.codename = PROJECT_CODENAME.to_string();
        fresh
    };
    config.ensure_workspace_dirs()?;
    config.save_to_workspace(workspace)?;
    Ok(config)
}

fn seed_repo(repo: &Path) -> Result<()> {
    fs::create_dir_all(repo).with_context(|| format!("failed creating repo {}", repo.display()))?;
    fs::create_dir_all(repo.join("docs"))
        .with_context(|| format!("failed creating {}", repo.join("docs").display()))?;

    write_if_missing(
        &repo.join("README.md"),
        "# Codex Session Smoke Repo\n\nThis disposable repo is used to exercise Topside Codex session lifecycle flows.\n",
    )?;
    write_if_missing(
        &repo.join("docs/overview.md"),
        "# Smoke Overview\n\nUse this repo to verify Topside can create, terminate, resume, and archive Codex sessions without leaving stale UI state behind.\n",
    )?;

    Ok(())
}

fn write_if_missing(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, contents).with_context(|| format!("failed writing {}", path.display()))
}
