use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct GitContext {
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub dirty: bool,
}

pub fn read_git_context(workspace_root: &Path) -> GitContext {
    let branch = run_git(workspace_root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let commit = run_git(workspace_root, &["rev-parse", "HEAD"]);
    let status = run_git(workspace_root, &["status", "--porcelain"]);

    GitContext {
        branch,
        commit,
        dirty: status.map(|v| !v.trim().is_empty()).unwrap_or(false),
    }
}

fn run_git(workspace_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(args)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}
