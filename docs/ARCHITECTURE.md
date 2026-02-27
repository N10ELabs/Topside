# Architecture

## Runtime

`n10e` is a single Rust binary with subcommands.

`n10e serve` runs:
- Axum HTTP server (workspace UI + mutation endpoints)
- stdio MCP server (JSON-RPC style tools)
- filesystem watcher for incremental reindexing
- SQLite index/search layer

## Storage Model

- Source of truth: markdown files on disk.
- Index/search layer: SQLite (`.n10e/index.sqlite`).
- Full-text search: SQLite FTS5 (`fts_documents`).
- Reverse references: `entity_links` from wiki links (`[[task:...]]`, `[[project:...]]`, `[[note:...]]`).

## Safety

- Writes are restricted to workspace root.
- Updates require `expected_revision` (optimistic lock).
- Deletion path is archive-only (`archive_entity` moves file under `archive/`).
- All app/MCP mutations emit immutable activity events (`activity_events`).

## Indexing

- Startup full scan (configurable).
- File watcher with debounce for incremental index updates.
- Stale DB paths are removed during full scan reconciliation.

## UI

- Askama server-rendered templates.
- htmx polling for workspace partials:
  - `/partials/tasks`
  - `/partials/notes`
  - `/partials/activity`
- ETag support on partial endpoints.
- The UI is designed as a local operator workbench for planning, context inspection, and agent handoff.
