# Architecture

## Runtime

n10e is a single Rust binary with subcommands.

n10e serve runs:

- Axum HTTP server (workspace UI + mutation endpoints)
- stdio MCP server (JSON-RPC style tools)
- filesystem watcher for incremental reindexing
- SQLite index/search layer

## Storage Model

- Source of truth: markdown files on disk.
- Index/search layer: SQLite (.n10e/index.sqlite).
- Full-text search: SQLite FTS5 (fts_documents).
- Reverse references: entity_links from wiki links ([[task:...]], [[project:...]], [[note:...]]).

## Safety

- Writes are restricted to workspace root.
- Updates require expected_revision (optimistic lock).
- Deletion path is archive-only (archive_entity moves file under archive/).
- All app/MCP mutations emit immutable activity events (activity_events).

## Indexing

- Startup full scan (configurable).
- File watcher with debounce for incremental index updates.
- Stale DB paths are removed during full scan reconciliation.

## Linked Source Sync

- Projects can carry a linked local folder or GitHub repository as source metadata.
- Phase 1 repo sync is manual and local-folder only.
- POST /api/projects/{id}/sync scans linked folders for to-do.md, todo.md, and TODO.md.
- Markdown checkboxes are imported into n10e as sync-managed tasks without writing back to repo files.
- Project metadata stores the last sync time and summary so the UI can surface sync state in Project Settings.

## UI

- Askama server-rendered shell template.
- Lightweight in-page JavaScript controller for the three-pane workspace.
- JSON endpoints drive project selection, task mutation/reorder, note editing, project linking, and manual project sync.
- The UI is designed as a shared project context hub: projects on the left, inline task planning in the center, and notes on the right.
