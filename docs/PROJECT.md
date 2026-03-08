# Project

Last updated: 2026-03-08

This is the canonical project overview for Topside. It consolidates the product, architecture, development, and compatibility context that used to be split across several docs.

## Positioning

Topside is an open source Rust AI-native project development layer for local software projects.

It is designed to keep project context durable, inspectable, and close to the repo while supporting fast local iteration with Codex. The core workflow is repo-adjacent rather than chat-first: tasks, notes, sync state, and agent session records live in the workspace and can be reconciled with local project files.

## What Topside Is

- a local-first project layer on top of real codebases
- a markdown-first system for planning, notes, and handoff state
- a Codex-oriented desktop/browser workspace for organizing ongoing project work

## What Topside Is Not

- a hosted orchestration dashboard
- a broad MCP tool catalog product
- a replacement for git, IDEs, or agent runtimes
- a multi-user auth/collaboration system in the current scope

## Current Product Shape

Topside is strongest today when:

- a project is linked to a local folder
- `docs/to-do.md` is used as the managed task surface
- repo docs are linked into Topside notes
- the macOS desktop shell is used to launch and manage Codex sessions

The standalone MCP server still exists, but it is intentionally narrow. The main product integration path is the workspace plus the Codex session layer.

## Architecture

Topside is a single Rust binary with multiple operating modes:

- `topside serve`: browser-first localhost UI and API server
- `topside open`: the same local UI inside the native macOS shell
- `topside mcp`: standalone protocol-compatibility MCP stdio server
- `topside dev`: file-watching development supervisor for `serve`
- `topside bundle-app`: macOS `.app` bundler

`serve` and `open` bootstrap the same workspace service stack:

- config load and workspace validation
- SQLite migrations and index bootstrap
- startup scan plus file watcher
- Axum HTTP server for the UI and JSON endpoints
- Codex session manager and port manager backing desktop-oriented flows

`mcp` is separate. It is not embedded under `serve`; it runs as its own stdio command.

## Storage Model

Topside is markdown-first.

- source of truth: markdown files in the workspace and selected linked repo files
- local index: `.topside/index.sqlite`
- search: SQLite FTS5
- backlinks: `entity_links` built from wiki-style links
- activity history: append-only SQLite activity log enriched with git context when available

Workspace layout:

- `projects/`
- `tasks/`
- `notes/`
- `agents/`
- `archive/`
- `.topside/`

## Sync And Codex Workflow

Topside currently supports:

- manual project scans of `to-do.md`, `todo.md`, and `TODO.md`
- managed task sync centered on `docs/to-do.md` plus `.topside-sync.json` sidecars
- linked note sync for repo markdown docs
- Codex session launch, discovery, resume, terminate, and PTY streaming for local linked projects in the macOS desktop shell

Topside’s stronger integration is local project context plus Codex session orchestration. Runtime project interaction happens through the workspace, sync flows, and desktop session management rather than through MCP tool calls.

## Development

Prerequisites:

- Rust stable
- macOS for `topside open`, app bundling, and the native desktop shell

Common commands:

```bash
topside init /path/to/workspace
topside --workspace /path/to/workspace serve
topside --workspace /path/to/workspace open
topside [--workspace /path/to/workspace] dev
cargo test
cargo test --test service_integration
cargo test --test http_integration
cargo test --test mcp_compat_integration
topside --workspace /path/to/workspace seed-bench --count 5000
topside --workspace /path/to/workspace bench --iterations 1000 --query benchmark-search-token
topside [--workspace /path/to/workspace] bundle-app --output-dir ./dist
./scripts/package-macos-release.sh --output-dir ./dist
```

Repository note:

- this repo contains both the app source and a dogfooded Topside workspace
- `docs/to-do.md` and `docs/.to-do.topside-sync.json` are live synced workspace files and should be maintained, not casually renamed or removed

## Compatibility

`topside mcp` is a protocol-compatibility endpoint, not a full operational MCP surface.

Current supported protocol methods:

- `initialize`
- `ping`
- `resources/list`
- `resources/templates/list`
- `tools/list`

Current behavior:

- `tools/list` returns an empty tool array
- `tools/call` returns `unknown tool name`
- direct operational methods return `method not found`

The current validation baseline is:

- automated profile simulation: done
- real client smoke test (Codex): protocol-only
- real client smoke test (Claude Code): pending

Run the compatibility suite with:

```bash
cargo test --test mcp_compat_integration
```

## Product Direction

Topside’s near-term priorities are:

- Codex workflow hardening
- better sync conflict presentation and resolution
- stronger project briefs and handoff artifacts
- cleaner release-quality distribution and contributor docs
- keeping the workspace and docs aligned with the real codebase

## Related Docs

- [README.md](../README.md)
- [SCHEMA.md](SCHEMA.md)
- [SYNC.md](SYNC.md)
- [MCP.md](MCP.md)
- [PERFORMANCE.md](PERFORMANCE.md)
- [MIGRATIONS.md](MIGRATIONS.md)
