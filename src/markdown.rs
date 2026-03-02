use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use pulldown_cmark::{Options, Parser, html};
use regex::Regex;
use serde_yaml::Value;
use sha2::{Digest, Sha256};

use crate::types::{
    EntityFrontmatter, EntityType, NoteFrontmatter, ParsedEntity, ProjectFrontmatter,
    TaskFrontmatter, WikiLink,
};

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n")
}

pub fn parse_entity_markdown(content: &str) -> Result<ParsedEntity> {
    let normalized = normalize_newlines(content);
    let (frontmatter_raw, body) = split_frontmatter(&normalized)?;

    let yaml_value: Value =
        serde_yaml::from_str(&frontmatter_raw).context("failed parsing frontmatter YAML")?;
    let entity_type = yaml_value
        .get("type")
        .and_then(Value::as_str)
        .and_then(EntityType::parse)
        .context("frontmatter missing valid type")?;

    let mut frontmatter = match entity_type {
        EntityType::Task => {
            EntityFrontmatter::Task(serde_yaml::from_str::<TaskFrontmatter>(&frontmatter_raw)?)
        }
        EntityType::Project => EntityFrontmatter::Project(serde_yaml::from_str::<
            ProjectFrontmatter,
        >(&frontmatter_raw)?),
        EntityType::Note => {
            EntityFrontmatter::Note(serde_yaml::from_str::<NoteFrontmatter>(&frontmatter_raw)?)
        }
    };

    let computed_revision = compute_revision(&frontmatter_raw, &body);
    if frontmatter.revision().is_empty() {
        frontmatter.set_revision(computed_revision.clone());
    }

    let links = extract_wiki_links(&body);

    Ok(ParsedEntity {
        frontmatter,
        body,
        revision: computed_revision,
        links,
    })
}

pub fn render_entity_markdown(frontmatter: &mut EntityFrontmatter, body: &str) -> Result<String> {
    let no_revision_yaml = render_frontmatter_yaml(frontmatter, None);
    let revision = compute_revision(&no_revision_yaml, body);
    frontmatter.set_revision(revision.clone());
    let full_yaml = render_frontmatter_yaml(frontmatter, Some(&revision));

    let body = if body.ends_with('\n') {
        body.to_string()
    } else {
        format!("{body}\n")
    };

    Ok(format!("---\n{full_yaml}---\n{body}"))
}

pub fn compute_revision(frontmatter_raw: &str, body: &str) -> String {
    let revision_re = Regex::new(r"(?m)^revision:\s*.*\n?").expect("valid revision regex");
    let cleaned = revision_re.replace_all(frontmatter_raw, "");
    let mut hasher = Sha256::new();
    hasher.update(cleaned.as_bytes());
    hasher.update(b"\n---\n");
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn split_frontmatter(content: &str) -> Result<(String, String)> {
    if !content.starts_with("---\n") {
        anyhow::bail!("markdown file missing opening frontmatter delimiter");
    }

    let rest = &content[4..];
    let end = rest
        .find("\n---\n")
        .context("markdown file missing closing frontmatter delimiter")?;

    let frontmatter = rest[..end].to_string();
    let body = rest[end + 5..].to_string();
    Ok((frontmatter, body))
}

pub fn extract_wiki_links(body: &str) -> Vec<WikiLink> {
    let re = Regex::new(r"\[\[(task|project|note):([^\]]+)\]\]").expect("valid wiki regex");
    re.captures_iter(body)
        .filter_map(|caps| {
            let kind = caps.get(1)?.as_str();
            let id = caps.get(2)?.as_str();
            Some(WikiLink {
                target_type: EntityType::parse(kind)?,
                target_id: id.to_string(),
                raw: caps.get(0)?.as_str().to_string(),
            })
        })
        .collect()
}

pub fn parse_optional_datetime(value: &str) -> Result<Option<DateTime<Utc>>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let parsed = DateTime::parse_from_rfc3339(trimmed)
        .with_context(|| format!("invalid RFC3339 datetime: {trimmed}"))?
        .with_timezone(&Utc);
    Ok(Some(parsed))
}

pub fn render_markdown_html(body: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);

    let parser = Parser::new_ext(body, options);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

fn render_frontmatter_yaml(frontmatter: &EntityFrontmatter, revision: Option<&str>) -> String {
    match frontmatter {
        EntityFrontmatter::Task(task) => {
            let mut out = String::new();
            out.push_str(&format!("id: {}\n", yaml_scalar(&task.id)));
            out.push_str("type: task\n");
            out.push_str(&format!("title: {}\n", yaml_scalar(&task.title)));
            out.push_str(&format!("project_id: {}\n", yaml_scalar(&task.project_id)));
            out.push_str(&format!("status: {}\n", task.status.as_str()));
            out.push_str(&format!("priority: {}\n", task.priority.as_str()));
            out.push_str(&format!("assignee: {}\n", yaml_scalar(&task.assignee)));
            if let Some(due_at) = task.due_at {
                out.push_str(&format!("due_at: {}\n", due_at.to_rfc3339()));
            }
            out.push_str(&format!("sort_order: {}\n", task.sort_order));
            if let Some(completed_at) = task.completed_at {
                out.push_str(&format!("completed_at: {}\n", completed_at.to_rfc3339()));
            }
            if let Some(sync_kind) = &task.sync_kind {
                out.push_str(&format!("sync_kind: {}\n", sync_kind.as_str()));
            }
            if let Some(sync_path) = &task.sync_path {
                out.push_str(&format!("sync_path: {}\n", yaml_scalar(sync_path)));
            }
            if let Some(sync_key) = &task.sync_key {
                out.push_str(&format!("sync_key: {}\n", yaml_scalar(sync_key)));
            }
            if task.sync_managed {
                out.push_str("sync_managed: true\n");
            }
            render_tags(&mut out, task.tags.as_ref());
            out.push_str(&format!("created_at: {}\n", task.created_at.to_rfc3339()));
            out.push_str(&format!("updated_at: {}\n", task.updated_at.to_rfc3339()));
            if let Some(revision) = revision {
                out.push_str(&format!("revision: {}\n", yaml_scalar(revision)));
            }
            out
        }
        EntityFrontmatter::Project(project) => {
            let mut out = String::new();
            out.push_str(&format!("id: {}\n", yaml_scalar(&project.id)));
            out.push_str("type: project\n");
            out.push_str(&format!("title: {}\n", yaml_scalar(&project.title)));
            out.push_str(&format!("status: {}\n", project.status.as_str()));
            if let Some(owner) = &project.owner {
                out.push_str(&format!("owner: {}\n", yaml_scalar(owner)));
            }
            if let Some(source_kind) = &project.source_kind {
                out.push_str(&format!("source_kind: {}\n", source_kind.as_str()));
            }
            if let Some(source_locator) = &project.source_locator {
                out.push_str(&format!(
                    "source_locator: {}\n",
                    yaml_scalar(source_locator)
                ));
            }
            if let Some(sync_source_key) = &project.sync_source_key {
                out.push_str(&format!(
                    "sync_source_key: {}\n",
                    yaml_scalar(sync_source_key)
                ));
            }
            if let Some(last_synced_at) = project.last_synced_at {
                out.push_str(&format!(
                    "last_synced_at: {}\n",
                    last_synced_at.to_rfc3339()
                ));
            }
            if let Some(last_sync_summary) = &project.last_sync_summary {
                out.push_str(&format!(
                    "last_sync_summary: {}\n",
                    yaml_scalar(last_sync_summary)
                ));
            }
            if let Some(task_sync_mode) = &project.task_sync_mode {
                out.push_str(&format!("task_sync_mode: {}\n", task_sync_mode.as_str()));
            }
            if let Some(task_sync_file) = &project.task_sync_file {
                out.push_str(&format!(
                    "task_sync_file: {}\n",
                    yaml_scalar(task_sync_file)
                ));
            }
            if project.task_sync_enabled {
                out.push_str("task_sync_enabled: true\n");
            }
            if let Some(task_sync_status) = &project.task_sync_status {
                out.push_str(&format!(
                    "task_sync_status: {}\n",
                    task_sync_status.as_str()
                ));
            }
            if let Some(task_sync_last_seen_hash) = &project.task_sync_last_seen_hash {
                out.push_str(&format!(
                    "task_sync_last_seen_hash: {}\n",
                    yaml_scalar(task_sync_last_seen_hash)
                ));
            }
            if let Some(task_sync_last_inbound_at) = project.task_sync_last_inbound_at {
                out.push_str(&format!(
                    "task_sync_last_inbound_at: {}\n",
                    task_sync_last_inbound_at.to_rfc3339()
                ));
            }
            if let Some(task_sync_last_outbound_at) = project.task_sync_last_outbound_at {
                out.push_str(&format!(
                    "task_sync_last_outbound_at: {}\n",
                    task_sync_last_outbound_at.to_rfc3339()
                ));
            }
            if let Some(task_sync_conflict_summary) = &project.task_sync_conflict_summary {
                out.push_str(&format!(
                    "task_sync_conflict_summary: {}\n",
                    yaml_scalar(task_sync_conflict_summary)
                ));
            }
            if let Some(task_sync_conflict_at) = project.task_sync_conflict_at {
                out.push_str(&format!(
                    "task_sync_conflict_at: {}\n",
                    task_sync_conflict_at.to_rfc3339()
                ));
            }
            render_tags(&mut out, project.tags.as_ref());
            out.push_str(&format!(
                "created_at: {}\n",
                project.created_at.to_rfc3339()
            ));
            out.push_str(&format!(
                "updated_at: {}\n",
                project.updated_at.to_rfc3339()
            ));
            if let Some(revision) = revision {
                out.push_str(&format!("revision: {}\n", yaml_scalar(revision)));
            }
            out
        }
        EntityFrontmatter::Note(note) => {
            let mut out = String::new();
            out.push_str(&format!("id: {}\n", yaml_scalar(&note.id)));
            out.push_str("type: note\n");
            out.push_str(&format!("title: {}\n", yaml_scalar(&note.title)));
            if let Some(project_id) = &note.project_id {
                out.push_str(&format!("project_id: {}\n", yaml_scalar(project_id)));
            }
            if let Some(sync_kind) = &note.sync_kind {
                out.push_str(&format!("sync_kind: {}\n", sync_kind.as_str()));
            }
            if let Some(sync_path) = &note.sync_path {
                out.push_str(&format!("sync_path: {}\n", yaml_scalar(sync_path)));
            }
            if let Some(sync_status) = &note.sync_status {
                out.push_str(&format!("sync_status: {}\n", sync_status.as_str()));
            }
            if let Some(sync_last_seen_hash) = &note.sync_last_seen_hash {
                out.push_str(&format!(
                    "sync_last_seen_hash: {}\n",
                    yaml_scalar(sync_last_seen_hash)
                ));
            }
            if let Some(sync_last_inbound_at) = note.sync_last_inbound_at {
                out.push_str(&format!(
                    "sync_last_inbound_at: {}\n",
                    sync_last_inbound_at.to_rfc3339()
                ));
            }
            if let Some(sync_last_outbound_at) = note.sync_last_outbound_at {
                out.push_str(&format!(
                    "sync_last_outbound_at: {}\n",
                    sync_last_outbound_at.to_rfc3339()
                ));
            }
            if let Some(sync_conflict_summary) = &note.sync_conflict_summary {
                out.push_str(&format!(
                    "sync_conflict_summary: {}\n",
                    yaml_scalar(sync_conflict_summary)
                ));
            }
            if let Some(sync_conflict_at) = note.sync_conflict_at {
                out.push_str(&format!(
                    "sync_conflict_at: {}\n",
                    sync_conflict_at.to_rfc3339()
                ));
            }
            render_tags(&mut out, note.tags.as_ref());
            out.push_str(&format!("created_at: {}\n", note.created_at.to_rfc3339()));
            out.push_str(&format!("updated_at: {}\n", note.updated_at.to_rfc3339()));
            if let Some(revision) = revision {
                out.push_str(&format!("revision: {}\n", yaml_scalar(revision)));
            }
            out
        }
    }
}

fn render_tags(out: &mut String, tags: Option<&Vec<String>>) {
    if let Some(tags) = tags {
        if tags.is_empty() {
            return;
        }
        out.push_str("tags:\n");
        for tag in tags {
            out.push_str(&format!("  - {}\n", yaml_scalar(tag)));
        }
    }
}

fn yaml_scalar(value: &str) -> String {
    let encoded = serde_yaml::to_string(value).unwrap_or_else(|_| value.to_string());
    encoded
        .trim_start_matches("---\n")
        .trim_end_matches('\n')
        .to_string()
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use crate::types::{
        EntityFrontmatter, EntityType, NoteFrontmatter, TaskFrontmatter, TaskPriority, TaskStatus,
    };

    use super::{parse_entity_markdown, render_entity_markdown};

    #[test]
    fn round_trip_task_revision() {
        let mut fm = EntityFrontmatter::Task(TaskFrontmatter {
            id: "tsk_1".to_string(),
            entity_type: EntityType::Task,
            title: "Test".to_string(),
            project_id: "prj_1".to_string(),
            status: TaskStatus::Todo,
            priority: TaskPriority::P2,
            assignee: "agent:codex".to_string(),
            due_at: None,
            sort_order: 1,
            completed_at: None,
            sync_kind: None,
            sync_path: None,
            sync_key: None,
            sync_managed: false,
            tags: Some(vec!["alpha".to_string()]),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            revision: String::new(),
        });
        let rendered = render_entity_markdown(&mut fm, "hello [[note:nte_1]]").unwrap();
        let parsed = parse_entity_markdown(&rendered).unwrap();
        assert_eq!(parsed.frontmatter.id(), "tsk_1");
        assert_eq!(parsed.links.len(), 1);
    }

    #[test]
    fn parse_without_revision_fills_computed() {
        let raw = r#"---
id: nte_1
type: note
title: note one
created_at: 2026-01-01T00:00:00Z
updated_at: 2026-01-01T00:00:00Z
---
body
"#;
        let parsed = parse_entity_markdown(raw).unwrap();
        assert!(!parsed.revision.is_empty());
        match parsed.frontmatter {
            EntityFrontmatter::Note(NoteFrontmatter { revision, .. }) => {
                assert!(!revision.is_empty());
            }
            _ => panic!("expected note"),
        }
    }

    #[test]
    fn markdown_to_html_renders_task_lists() {
        let html = super::render_markdown_html("- [x] done");
        assert!(html.contains("checkbox"));
    }
}
