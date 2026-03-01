use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::types::{TaskItem, TaskStatus};

pub const DEFAULT_MANAGED_TASK_SYNC_FILE: &str = "docs/to-do.md";
pub const LEGACY_MANAGED_TASK_SYNC_FILE: &str = "to-do.md";
pub const OUTBOUND_DEBOUNCE_MS: u64 = 400;
pub const WATCHER_DEBOUNCE_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub enum ManagedTodoEntryKind {
    Section,
    Task { completed: bool },
}

#[derive(Debug, Clone)]
pub struct ParsedManagedTodoEntry {
    pub title: String,
    pub kind: ManagedTodoEntryKind,
    pub sync_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedTodoRenderEntry {
    pub title: String,
    pub kind: ManagedTodoEntryKind,
    pub sync_key: String,
}

#[derive(Debug, Clone, Default)]
pub struct ManagedTodoParseResult {
    pub entries: Vec<ParsedManagedTodoEntry>,
    pub had_inline_sync_keys: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManagedTodoSidecar {
    #[serde(default = "managed_todo_sidecar_version")]
    pub version: u8,
    #[serde(default)]
    pub entries: Vec<ManagedTodoRenderEntry>,
}

pub fn parse_managed_todo(
    content: &str,
    sidecar_entries: &[ManagedTodoRenderEntry],
) -> ManagedTodoParseResult {
    let mut out = Vec::new();
    let mut raw_entries = Vec::new();
    let mut had_inline_sync_keys = false;

    for line in content.replace("\r\n", "\n").lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(title) = parse_section_title(trimmed) {
            let (visible_title, existing_id) = split_visible_title_and_id(title);
            let normalized = visible_title.trim();
            if normalized.is_empty() {
                continue;
            }
            had_inline_sync_keys = had_inline_sync_keys || existing_id.is_some();
            raw_entries.push(RawManagedTodoEntry {
                title: normalized.to_string(),
                kind: ManagedTodoEntryKind::Section,
                inline_sync_key: existing_id,
            });
            continue;
        }

        let Some((completed, title)) = parse_checkbox_title(trimmed) else {
            continue;
        };
        let (visible_title, existing_id) = split_visible_title_and_id(title);
        let normalized = visible_title.trim();
        if normalized.is_empty() {
            continue;
        }
        let kind = ManagedTodoEntryKind::Task { completed };
        had_inline_sync_keys = had_inline_sync_keys || existing_id.is_some();
        raw_entries.push(RawManagedTodoEntry {
            title: normalized.to_string(),
            kind,
            inline_sync_key: existing_id,
        });
    }

    let mut available = sidecar_entries
        .iter()
        .cloned()
        .map(Some)
        .collect::<Vec<_>>();

    for raw in &raw_entries {
        if let Some(sync_key) = raw.inline_sync_key.as_deref() {
            if let Some(index) = available.iter().position(|entry| {
                entry
                    .as_ref()
                    .map(|candidate| candidate.sync_key == sync_key)
                    .unwrap_or(false)
            }) {
                available[index] = None;
            }
        }
    }

    for (index, raw) in raw_entries.into_iter().enumerate() {
        let sync_key = match raw.inline_sync_key {
            Some(sync_key) => sync_key,
            None => match_sidecar_sync_key(index, &raw, &mut available)
                .unwrap_or_else(|| generate_sync_key_for_kind(&raw.kind)),
        };
        out.push(ParsedManagedTodoEntry {
            title: raw.title,
            kind: raw.kind,
            sync_key,
        });
    }

    ManagedTodoParseResult {
        entries: out,
        had_inline_sync_keys,
    }
}

pub fn render_managed_todo(entries: &[ManagedTodoRenderEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    for entry in entries {
        match &entry.kind {
            ManagedTodoEntryKind::Section => {
                out.push_str("## ");
                out.push_str(entry.title.trim());
            }
            ManagedTodoEntryKind::Task { completed } => {
                out.push_str(if *completed { "- [x] " } else { "- [ ] " });
                out.push_str(entry.title.trim());
            }
        }
        out.push('\n');
    }
    out
}

pub fn parse_managed_todo_sidecar(content: &str) -> Result<ManagedTodoSidecar> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(ManagedTodoSidecar {
            version: managed_todo_sidecar_version(),
            entries: Vec::new(),
        });
    }
    serde_json::from_str(trimmed).context("failed parsing managed task sync sidecar")
}

pub fn render_managed_todo_sidecar(entries: &[ManagedTodoRenderEntry]) -> Result<String> {
    let sidecar = ManagedTodoSidecar {
        version: managed_todo_sidecar_version(),
        entries: entries.to_vec(),
    };
    let mut rendered =
        serde_json::to_string_pretty(&sidecar).context("failed rendering managed task sync sidecar")?;
    rendered.push('\n');
    Ok(rendered)
}

pub fn compute_file_hash(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn compute_file_hash_from_path(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("failed reading {}", path.display()))?;
    Ok(Some(compute_file_hash(&content)))
}

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    fs::create_dir_all(parent)
        .with_context(|| format!("failed creating parent directory {}", parent.display()))?;
    Ok(())
}

pub fn resolve_managed_file_path(source_root: &str, relative_path: &str) -> Result<PathBuf> {
    let relative = Path::new(relative_path);
    if relative.is_absolute() {
        anyhow::bail!("managed task sync file must be relative to the linked source folder");
    }
    for component in relative.components() {
        if matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            anyhow::bail!("managed task sync file must stay inside the linked source folder");
        }
    }
    Ok(PathBuf::from(source_root).join(relative))
}

pub fn managed_todo_sidecar_path(managed_file_path: &Path) -> PathBuf {
    let parent = managed_file_path.parent().unwrap_or_else(|| Path::new(""));
    let stem = managed_file_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("to-do");
    parent.join(format!(".{stem}.n10e-sync.json"))
}

pub fn normalize_managed_task_sync_file(sync_file: Option<&str>) -> String {
    match sync_file.map(str::trim) {
        Some("") | None => DEFAULT_MANAGED_TASK_SYNC_FILE.to_string(),
        Some(LEGACY_MANAGED_TASK_SYNC_FILE) => DEFAULT_MANAGED_TASK_SYNC_FILE.to_string(),
        Some(value) => value.to_string(),
    }
}

pub fn is_heading_title(title: &str) -> bool {
    let trimmed = title.trim();
    trimmed.starts_with("## ")
}

pub fn visible_heading_title(title: &str) -> String {
    title.trim().trim_start_matches("##").trim().to_string()
}

pub fn task_title_from_entry(entry: &ParsedManagedTodoEntry) -> String {
    match entry.kind {
        ManagedTodoEntryKind::Section => format!("## {}", entry.title.trim()),
        ManagedTodoEntryKind::Task { .. } => entry.title.trim().to_string(),
    }
}

pub fn render_entry_from_task(task: &TaskItem) -> Option<ManagedTodoRenderEntry> {
    let sync_key = task.sync_key.clone()?;
    if is_heading_title(&task.title) {
        return Some(ManagedTodoRenderEntry {
            title: visible_heading_title(&task.title),
            kind: ManagedTodoEntryKind::Section,
            sync_key,
        });
    }

    Some(ManagedTodoRenderEntry {
        title: task.title.trim().to_string(),
        kind: ManagedTodoEntryKind::Task {
            completed: task.status == TaskStatus::Done,
        },
        sync_key,
    })
}

pub fn ensure_sync_key_for_title(existing: Option<&str>, title: &str) -> String {
    let desired_prefix = if is_heading_title(title) { "sec_" } else { "tsk_" };
    match existing {
        Some(value) if value.starts_with(desired_prefix) => value.to_string(),
        _ => format!("{desired_prefix}{}", Ulid::new()),
    }
}

pub fn generate_sync_key_for_kind(kind: &ManagedTodoEntryKind) -> String {
    match kind {
        ManagedTodoEntryKind::Section => format!("sec_{}", Ulid::new()),
        ManagedTodoEntryKind::Task { .. } => format!("tsk_{}", Ulid::new()),
    }
}

fn parse_section_title(line: &str) -> Option<&str> {
    line.strip_prefix("## ")
}

fn parse_checkbox_title(line: &str) -> Option<(bool, &str)> {
    line.strip_prefix("- [ ] ")
        .map(|title| (false, title))
        .or_else(|| line.strip_prefix("- [x] ").map(|title| (true, title)))
        .or_else(|| line.strip_prefix("- [X] ").map(|title| (true, title)))
        .or_else(|| line.strip_prefix("* [ ] ").map(|title| (false, title)))
        .or_else(|| line.strip_prefix("* [x] ").map(|title| (true, title)))
        .or_else(|| line.strip_prefix("* [X] ").map(|title| (true, title)))
}

fn split_visible_title_and_id(value: &str) -> (&str, Option<String>) {
    let re = Regex::new(r"(?i)\s*<!--\s*n10e:id=([A-Za-z0-9_-]+)\s*-->\s*$")
        .expect("valid managed task sync id regex");
    if let Some(captures) = re.captures(value) {
        let matched = captures.get(0).map(|m| m.as_str()).unwrap_or("");
        let id = captures.get(1).map(|m| m.as_str().to_string());
        let visible = value
            .strip_suffix(matched)
            .unwrap_or(value)
            .trim_end();
        return (visible, id);
    }
    (value, None)
}

fn match_sidecar_sync_key(
    index: usize,
    raw: &RawManagedTodoEntry,
    available: &mut [Option<ManagedTodoRenderEntry>],
) -> Option<String> {
    let normalized_title = normalize_entry_title(&raw.title);

    if let Some(found) = take_matching_sidecar_entry(available, |candidate| {
        same_entry_kind(&candidate.kind, &raw.kind)
            && normalize_entry_title(&candidate.title) == normalized_title
            && candidate.kind == raw.kind
    }) {
        return Some(found.sync_key);
    }

    if let Some(found) = take_matching_sidecar_entry(available, |candidate| {
        same_entry_kind(&candidate.kind, &raw.kind)
            && normalize_entry_title(&candidate.title) == normalized_title
    }) {
        return Some(found.sync_key);
    }

    if let Some(candidate) = available.get_mut(index).and_then(Option::take) {
        return Some(candidate.sync_key);
    }

    take_matching_sidecar_entry(available, |candidate| same_entry_kind(&candidate.kind, &raw.kind))
        .map(|entry| entry.sync_key)
}

fn take_matching_sidecar_entry(
    available: &mut [Option<ManagedTodoRenderEntry>],
    predicate: impl Fn(&ManagedTodoRenderEntry) -> bool,
) -> Option<ManagedTodoRenderEntry> {
    let index = available.iter().position(|entry| {
        entry
            .as_ref()
            .map(|candidate| predicate(candidate))
            .unwrap_or(false)
    })?;
    available[index].take()
}

fn same_entry_kind(left: &ManagedTodoEntryKind, right: &ManagedTodoEntryKind) -> bool {
    matches!(
        (left, right),
        (ManagedTodoEntryKind::Section, ManagedTodoEntryKind::Section)
            | (ManagedTodoEntryKind::Task { .. }, ManagedTodoEntryKind::Task { .. })
    )
}

fn normalize_entry_title(value: &str) -> String {
    value
        .trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn managed_todo_sidecar_version() -> u8 {
    1
}

#[derive(Debug, Clone)]
struct RawManagedTodoEntry {
    title: String,
    kind: ManagedTodoEntryKind,
    inline_sync_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        ManagedTodoEntryKind, ManagedTodoRenderEntry, managed_todo_sidecar_path, parse_managed_todo,
        parse_managed_todo_sidecar, render_managed_todo, render_managed_todo_sidecar,
    };

    #[test]
    fn parse_managed_todo_reads_sections_and_tasks() {
        let parsed = parse_managed_todo(
            "## Planning <!-- n10e:id=sec_123 -->\n- [ ] Draft copy <!-- n10e:id=tsk_456 -->\n- [x] Ship it\n",
            &[],
        );

        assert_eq!(parsed.entries.len(), 3);
        assert_eq!(parsed.entries[0].kind, ManagedTodoEntryKind::Section);
        assert_eq!(parsed.entries[0].title, "Planning");
        assert!(parsed.had_inline_sync_keys);
        assert_eq!(
            parsed.entries[1].kind,
            ManagedTodoEntryKind::Task { completed: false }
        );
        assert_eq!(parsed.entries[1].sync_key, "tsk_456");
    }

    #[test]
    fn render_managed_todo_is_deterministic() {
        let content = render_managed_todo(&[
            ManagedTodoRenderEntry {
                title: "Planning".to_string(),
                kind: ManagedTodoEntryKind::Section,
                sync_key: "sec_123".to_string(),
            },
            ManagedTodoRenderEntry {
                title: "Draft copy".to_string(),
                kind: ManagedTodoEntryKind::Task { completed: false },
                sync_key: "tsk_456".to_string(),
            },
        ]);

        assert_eq!(
            content,
            "## Planning\n- [ ] Draft copy\n"
        );
    }

    #[test]
    fn parse_managed_todo_uses_sidecar_for_clean_file() {
        let parsed = parse_managed_todo(
            "## Planning\n- [ ] Draft copy\n",
            &[
                ManagedTodoRenderEntry {
                    title: "Planning".to_string(),
                    kind: ManagedTodoEntryKind::Section,
                    sync_key: "sec_123".to_string(),
                },
                ManagedTodoRenderEntry {
                    title: "Draft copy".to_string(),
                    kind: ManagedTodoEntryKind::Task { completed: false },
                    sync_key: "tsk_456".to_string(),
                },
            ],
        );

        assert!(!parsed.had_inline_sync_keys);
        assert_eq!(parsed.entries[0].sync_key, "sec_123");
        assert_eq!(parsed.entries[1].sync_key, "tsk_456");
    }

    #[test]
    fn sidecar_round_trips() {
        let rendered = render_managed_todo_sidecar(&[ManagedTodoRenderEntry {
            title: "Planning".to_string(),
            kind: ManagedTodoEntryKind::Section,
            sync_key: "sec_123".to_string(),
        }])
        .expect("sidecar renders");

        let parsed = parse_managed_todo_sidecar(&rendered).expect("sidecar parses");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].sync_key, "sec_123");
    }

    #[test]
    fn sidecar_path_is_hidden_beside_managed_file() {
        let path = managed_todo_sidecar_path(Path::new("docs/to-do.md"));
        assert_eq!(path.to_string_lossy(), "docs/.to-do.n10e-sync.json");
    }
}
