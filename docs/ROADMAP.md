# Roadmap

Last updated: 2026-03-08

## Current State

Topside already has the end-to-end local product loop in place:

- markdown-first workspace model
- SQLite indexing and search
- local linked-project sync primitives
- browser and macOS desktop modes
- Codex session launch, discovery, resume, and PTY streaming
- protocol-only standalone MCP compatibility command

## Active Priorities

### 1. Codex Workflow Hardening

- make Codex setup and binary discovery clearer
- improve session assignment and task-to-session flows
- reduce redundant session metadata and tighten the desktop UX

### 2. Sync Reliability And UX

- improve conflict presentation for managed task sync and linked note sync
- harden writeback and reconciliation behavior around local file edits
- make the `docs/to-do.md` workflow clearer and less surprising

### 3. Project Handoff Quality

- add stronger project briefs and task-focused context packs
- surface agent-readable summaries and handoff artifacts
- improve how project docs and notes are organized for pickup

### 4. Validation And Release Readiness

- complete real-client compatibility coverage, especially beyond protocol-only MCP handshakes
- tighten integration and regression testing
- ship cleaner release artifacts and contributor guidance

### 5. Workspace Cleanup

- keep documentation aligned with the current codebase
- reduce stale planning artifacts in the repo
- preserve the files the app actively syncs, especially `docs/to-do.md`

## Near-Term Checks

- verify real-world Codex compatibility in the current desktop workflow
- keep the benchmark harness current as the workspace model grows
- document contributor expectations around synced workspace files

## Done Foundations

- single Rust binary and local workspace bootstrap
- archive-only delete model and optimistic locking
- FTS-backed local search and backlinks
- managed `docs/to-do.md` task sync with sidecar metadata
- linked note sync for repo markdown files
- macOS app bundling and release workflow scaffolding
