# Markdown Schema

Last updated: 2026-03-08

Topside uses YAML frontmatter plus markdown body for workspace records.

## Core Entity Types

### Task

Required fields:

- `id` (`tsk_<ULID>`)
- `type: task`
- `title`
- `project_id`
- `status` (`backlog|todo|in_progress|blocked|done`)
- `priority` (`P0|P1|P2|P3`)
- `assignee` (`human:<id>` or `agent:<id>`)
- `created_at` (UTC ISO-8601)
- `updated_at` (UTC ISO-8601)
- `revision` (content hash)

Optional fields:

- `due_at` (UTC ISO-8601)
- `sort_order` (manual ordering for active task lists)
- `completed_at` (UTC ISO-8601)
- `sync_kind` (`repo_markdown|managed_todo_file`)
- `sync_path` (repo-relative path for synced tasks)
- `sync_key` (stable sync identity)
- `sync_managed` (`true` when the task participates in managed sync)
- `tags` (string list)

### Project

Required fields:

- `id` (`prj_<ULID>`)
- `type: project`
- `title`
- `status` (`active|paused|archived`)
- `created_at` (UTC ISO-8601)
- `updated_at` (UTC ISO-8601)
- `revision` (content hash)

Optional fields:

- `owner`
- `icon`
- `source_kind` (`local|github`)
- `source_locator` (linked local path or GitHub repo URL)
- `sync_source_key` (derived machine-facing source identity)
- `last_synced_at` (UTC ISO-8601)
- `last_sync_summary`
- `task_sync_mode` (`managed_todo_file`)
- `task_sync_file` (repo-relative managed task file, default `docs/to-do.md`)
- `task_sync_enabled` (`true|false`)
- `task_sync_status` (`live|paused|conflict`)
- `task_sync_last_seen_hash`
- `task_sync_last_inbound_at` (UTC ISO-8601)
- `task_sync_last_outbound_at` (UTC ISO-8601)
- `task_sync_conflict_summary`
- `task_sync_conflict_at` (UTC ISO-8601)
- `tags` (string list)

### Note

Required fields:

- `id` (`nte_<ULID>`)
- `type: note`
- `title`
- `created_at` (UTC ISO-8601)
- `updated_at` (UTC ISO-8601)
- `revision` (content hash)

Optional fields:

- `project_id`
- `sync_kind` (`repo_markdown`)
- `sync_path` (repo-relative path for linked markdown notes)
- `sync_status` (`live|conflict`)
- `sync_last_seen_hash`
- `sync_last_inbound_at` (UTC ISO-8601)
- `sync_last_outbound_at` (UTC ISO-8601)
- `sync_conflict_summary`
- `sync_conflict_at` (UTC ISO-8601)
- `tags` (string list)

## Codex Session Records

Codex session records live under `agents/<project-id>/` and use a dedicated frontmatter type.

Required fields:

- `id` (`ags_<ULID>`)
- `type: codex_session`
- `project_id`
- `title`
- `origin` (`topside|discovered`)
- `status` (`launching|live|resumable`)
- `cwd`
- `started_at` (UTC ISO-8601)
- `last_seen_at` (UTC ISO-8601)

Optional fields:

- `task_id`
- `codex_session_id`
- `ended_at` (UTC ISO-8601)

The markdown body stores the human-readable summary for the session.

## Revisions

- revisions are content hashes
- writes use optimistic locking against the last known revision
- sync flows also track file hashes separately for conflict detection

## Linking

Entity relationships inside markdown bodies use wiki-style links:

- `[[task:TASK_ID]]`
- `[[project:PROJECT_ID]]`
- `[[note:NOTE_ID]]`

Backlinks are indexed into SQLite `entity_links`.
