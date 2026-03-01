use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const TODO_FILE_NAMES: &[&str] = &["to-do.md", "todo.md", "TODO.md"];

#[derive(Debug, Clone)]
pub struct RepoTaskCandidate {
    pub relative_path: String,
    pub title: String,
    pub completed: bool,
    pub sync_key: String,
    pub section_path: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RepoSyncScan {
    pub files_scanned: usize,
    pub task_candidates: Vec<RepoTaskCandidate>,
}

pub fn derive_sync_source_key(kind: &str, locator: &str) -> String {
    match kind {
        "github" => {
            let normalized = locator
                .trim()
                .trim_end_matches('/')
                .trim_end_matches(".git")
                .to_lowercase();
            format!("github:{normalized}")
        }
        _ => format!("local:{}", locator.trim()),
    }
}

pub fn scan_repo_todo_files(root: &Path) -> Result<RepoSyncScan> {
    let mut files_scanned = 0usize;
    let mut task_candidates = Vec::new();

    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !is_ignored(entry.path()))
    {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if !is_supported_todo_file(entry.path()) {
            continue;
        }

        files_scanned += 1;
        let relative_path = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();
        task_candidates.extend(parse_todo_file(entry.path(), &relative_path)?);
    }

    Ok(RepoSyncScan {
        files_scanned,
        task_candidates,
    })
}

pub fn render_synced_task_body(relative_path: &str, section_path: &[String]) -> String {
    let mut body = format!("Synced from repo file `{relative_path}`.");
    if !section_path.is_empty() {
        body.push_str("\n\n");
        body.push_str("Section: ");
        body.push_str(&section_path.join(" / "));
        body.push('.');
    }
    body
}

fn parse_todo_file(path: &Path, relative_path: &str) -> Result<Vec<RepoTaskCandidate>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading repo todo file {}", path.display()))?;

    let mut headings = Vec::<String>::new();
    let mut out = Vec::new();

    for (line_index, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if let Some((level, title)) = parse_heading(trimmed) {
            headings.truncate(level.saturating_sub(1));
            headings.push(title.to_string());
            continue;
        }

        let Some((completed, title)) = parse_checkbox(trimmed) else {
            continue;
        };

        let normalized_title = normalize_title(title);
        if normalized_title.is_empty() {
            continue;
        }

        let line_number = line_index + 1;
        let sync_key = compute_short_hash(&format!("{relative_path}\nline:{line_number}"));

        out.push(RepoTaskCandidate {
            relative_path: relative_path.to_string(),
            title: title.trim().to_string(),
            completed,
            sync_key,
            section_path: headings.clone(),
        });
    }

    Ok(out)
}

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&ch| ch == '#').count();
    if level == 0 {
        return None;
    }

    let title = line[level..].trim();
    if title.is_empty() {
        return None;
    }

    Some((level, title))
}

fn parse_checkbox(line: &str) -> Option<(bool, &str)> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("- [ ] ")
        .or_else(|| trimmed.strip_prefix("* [ ] "))
        .map(|title| (false, title))
        .or_else(|| trimmed.strip_prefix("- [x] ").map(|title| (true, title)))
        .or_else(|| trimmed.strip_prefix("* [x] ").map(|title| (true, title)))
        .or_else(|| trimmed.strip_prefix("- [X] ").map(|title| (true, title)))
        .or_else(|| trimmed.strip_prefix("* [X] ").map(|title| (true, title)))?;
    Some(rest)
}

fn normalize_title(value: &str) -> String {
    value
        .trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn compute_short_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::new();
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_supported_todo_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| TODO_FILE_NAMES.iter().any(|candidate| candidate == &name))
        .unwrap_or(false)
}

fn is_ignored(path: &Path) -> bool {
    path.components().any(|part| {
        let value = part.as_os_str().to_string_lossy();
        value == ".git" || value == ".n10e"
    })
}
#[cfg(test)]
mod tests {
    use super::{TODO_FILE_NAMES, scan_repo_todo_files};
    use anyhow::Result;

    #[test]
    fn scan_repo_todo_files_discovers_markdown_checklists() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let repo_root = tmp.path();
        std::fs::create_dir_all(repo_root.join("docs"))?;
        std::fs::write(
            repo_root.join(TODO_FILE_NAMES[0]),
            "# Launch\n- [ ] Pick a name\n- [x] Ship it\n",
        )?;
        std::fs::write(
            repo_root.join("docs").join("todo.md"),
            "- [ ] Nested task\n",
        )?;
        std::fs::write(repo_root.join("README.md"), "- [ ] Ignore me\n")?;

        let scan = scan_repo_todo_files(repo_root)?;

        assert_eq!(scan.files_scanned, 2);
        assert_eq!(scan.task_candidates.len(), 3);
        assert_eq!(
            scan.task_candidates[0].section_path,
            vec!["Launch".to_string()]
        );
        assert_eq!(scan.task_candidates[1].title, "Ship it");
        assert!(scan.task_candidates[1].completed);

        Ok(())
    }

    #[test]
    fn sync_key_stays_stable_when_checkbox_text_changes_on_same_line() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let todo_path = tmp.path().join("to-do.md");

        std::fs::write(&todo_path, "- [ ] First title\n")?;
        let first_scan = scan_repo_todo_files(tmp.path())?;
        let first_key = first_scan.task_candidates[0].sync_key.clone();

        std::fs::write(&todo_path, "- [x] Renamed title\n")?;
        let second_scan = scan_repo_todo_files(tmp.path())?;
        let second = &second_scan.task_candidates[0];

        assert_eq!(first_key, second.sync_key);
        assert_eq!(second.title, "Renamed title");
        assert!(second.completed);

        Ok(())
    }
}
