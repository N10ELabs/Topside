---
id: nte_01KJP1KPJRN6ZJNG464RXY05KK
type: note
title: ARCHITECTURE.md
project_id: prj_01KJEH7ERYN87F9NFNZW7N12A4
sync_kind: repo_markdown
sync_path: docs/ARCHITECTURE.md
sync_status: live
sync_last_seen_hash: 7e7a2009b02d25cf64f24381d2b773a3a22ec769b8a69ef92cccf32dba327152
sync_last_inbound_at: 2026-03-04T01:07:12.824832+00:00
sync_last_outbound_at: 2026-03-02T03:10:34.822009+00:00
created_at: 2026-03-02T01:11:41.400615+00:00
updated_at: 2026-03-04T01:07:12.824832+00:00
revision: f064fa82d90e925efc19dae37af55a46b3cbe11f68e688952a3a390a42052d5c
---
# Architecture

## Runtime

Topside is a single Rust binary with subcommands.

topside serve runs:

- Axum HTTP server (workspace UI + mutation endpoints)
- stdio MCP server (JSON-RPC style tools)
- filesystem watcher for incremental reindexing
- SQLite index/search layer

## Storage Model

- Source of truth: markdown files on disk.
- Index/search layer: SQLite (.topside/index.sqlite).
- Full-text search: SQLite FTS5 (fts_documents).
- Reverse references: entity_links from wiki links ([[task:…]], [[project:…]], [[note:…]]).

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
- Markdown checkboxes are imported into Topside as sync-managed tasks without writing back to repo files.
- Project metadata stores the last sync time and summary so the UI can surface sync state in Project Settings.

## UI

- Askama server-rendered shell template.
- Lightweight in-page JavaScript controller for the three-pane workspace.
- JSON endpoints drive project selection, task mutation/reorder, note editing, project linking, and manual project sync.
- The UI is designed as a shared project context hub: projects on the left, inline task planning in the center, and notes on the right.
