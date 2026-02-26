# Markdown Schema

All entities use YAML frontmatter + markdown body.

## Task

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

Optional:
- `due_at` (UTC ISO-8601)
- `tags` (list)

## Project

Required:
- `id` (`prj_<ULID>`)
- `type: project`
- `title`
- `status` (`active|paused|archived`)
- `created_at`
- `updated_at`
- `revision`

Optional:
- `owner`
- `tags`

## Note

Required:
- `id` (`nte_<ULID>`)
- `type: note`
- `title`
- `created_at`
- `updated_at`
- `revision`

Optional:
- `project_id`
- `tags`

## Linking

Entity relationships in body markdown use wiki-style links:
- `[[task:TASK_ID]]`
- `[[project:PROJECT_ID]]`
- `[[note:NOTE_ID]]`

Backlinks are indexed into SQLite `entity_links`.

