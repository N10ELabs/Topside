# SQLite Migrations

Migrations are versioned SQL scripts applied on startup.

## Strategy

- Table: `schema_migrations`
- Forward-only migrations
- Applied during service bootstrap and `doctor`

## Current migration

- `001_base`: creates core tables and indexes
  - `files`
  - `entities`
  - `tasks`
  - `projects`
  - `notes`
  - `entity_links`
  - `activity_events`
  - `fts_documents` (FTS5)

## Operational guidance

- Do not mutate historical migration files after release.
- Add new migration IDs in increasing order (`002_*`, `003_*`, ...).
- Keep migrations idempotent where possible.

