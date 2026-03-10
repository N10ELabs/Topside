# Topside

<img src="Screenshot 2026-03-10 at 4.50.08 PM.png" alt="Topside" width="420" />

Agentic Building Layer | Integrated with Codex | OSS | Rust

It keeps project context in markdown on disk, indexes that context into SQLite for fast local retrieval, syncs selected repo files into a Topside workspace, and layers a focused browser/macOS desktop workbench on top. The current product is built for rapid local iteration with Codex: plan work, sync `docs/to-do.md`, link repo docs into notes, and launch or recover Codex sessions against the linked project from the Topside desktop shell.

## Current Status

Topside is in active pre-1.0 development. What exists in the codebase today:

- single Rust binary: `topside`
- local-first workspace storage for projects, tasks, notes, archived items, and agent session records
- Axum + Askama UI with project, task, note, archive, and Codex session flows
- SQLite + FTS5 indexing, reverse wiki-link lookup, and append-only activity history
- project source linking for local folders and GitHub repo metadata
- managed task sync for local projects, centered on `docs/to-do.md` plus `.topside-sync.json` sidecars
- linked note sync for repo markdown docs with conflict detection and resolution
- Codex session launch, discovery, resume, terminate, and PTY streaming for local projects inside the macOS desktop app
- browser mode (`serve`), macOS desktop mode (`open`), and macOS app bundling (`bundle-app`)
- standalone `topside mcp` protocol-compatibility command for MCP handshakes and discovery tests

## Scope

Topside is opinionated about staying local, inspectable, and repo-adjacent.

- single-user, local-first workflow
- markdown files on disk are the source of truth
- built to sit next to a real codebase, not replace the repo or the agent runtime
- not a cloud orchestration dashboard
- not a multi-user auth/collaboration system
- not a full remote GitHub sync engine today

Important current constraints:

- macOS is the first-class desktop target
- `topside open` and live Codex terminals are macOS-only
- live Codex sessions are available only for projects linked to a local folder
- GitHub-linked projects are currently source metadata, not full remote workspaces
- `topside mcp` is intentionally minimal right now; it is protocol-compatible, but it does not expose an operational tool surface

## Codex Workflow

For local projects opened in the desktop shell, Topside can:

- build a project/task context pack before launching Codex
- launch Codex in the linked repo root
- stream the terminal in-app with `xterm.js`
- persist Topside-side session records under `agents/<project-id>/`
- discover and reconnect prior Codex sessions associated with the same repo root

This is the main "Topside + Codex CLI" path today. The stronger integration is session orchestration and project context, not a large standalone MCP tool catalog.

## Install

From Homebrew on macOS:

```bash
brew tap N10ELabs/Topside https://github.com/N10ELabs/Topside
brew install N10ELabs/Topside/topside
```

From source:

```bash
cargo install --path .
```

Quick start:

```bash
topside init /path/to/workspace
topside --workspace /path/to/workspace doctor
topside --workspace /path/to/workspace serve
topside --workspace /path/to/workspace open
```

`serve` runs the local browser UI on `http://127.0.0.1:7410` by default. `open` launches the same local UI inside the native macOS shell and is the mode that exposes live Codex session controls.

If you want embedded Codex sessions, install the `codex` CLI first. `open` and live session management are macOS-only.

## CLI

```bash
topside init [PATH]
topside --workspace <PATH> serve
topside --workspace <PATH> open
topside [--workspace <PATH>] bundle-app --output-dir ./dist [--icon /path/to/topside.icns]
topside dev
topside --workspace <PATH> reindex
topside --workspace <PATH> import <SOURCE_PATH>
topside --workspace <PATH> doctor
topside --workspace <PATH> bench --iterations 200 --query task
topside --workspace <PATH> seed-bench --count 5000
topside --workspace <PATH> mcp
```

## Workspace Layout

`topside init` creates:

- `topside.toml`
- `projects/`
- `tasks/`
- `notes/`
- `agents/`
- `archive/`
- `.topside/index.sqlite`

For local linked projects, Topside may also manage repo-side files such as:

- `docs/to-do.md`
- `docs/.to-do.topside-sync.json`
- linked `docs/**/*.md` note targets

## Repository Layout

This repository currently contains both the application source and a dogfooded Topside workspace. The main top-level areas are:

- `src/` Rust application code
- `templates/` Askama shell template and in-page UI controller
- `tests/` integration tests for service, HTTP, and MCP compatibility paths
- `docs/` product and technical docs
- `examples/` sample `topside.toml`
- `scripts/` dev and macOS packaging helpers
- `vendor/xterm/` terminal assets used by the Codex pane
- `projects/`, `tasks/`, `notes/`, `agents/`, `archive/` workspace data for this repo's own Topside instance

If you are scanning the tree for implementation entry points, start with:

- `src/service.rs` for workspace behavior and sync logic
- `src/http.rs` for the UI/API surface
- `src/codex.rs` for Codex session management
- `src/task_sync.rs` and `src/repo_sync.rs` for repo-linked sync behavior
- `src/desktop.rs` for the native macOS shell

## Reference Docs

- project overview: [docs/PROJECT.md](docs/PROJECT.md)
- config example: [examples/topside.toml](examples/topside.toml)
- schema: [docs/SCHEMA.md](docs/SCHEMA.md)
- sync model: [docs/SYNC.md](docs/SYNC.md)
- MCP compatibility surface: [docs/MCP.md](docs/MCP.md)
- performance harness: [docs/PERFORMANCE.md](docs/PERFORMANCE.md)
- roadmap: [docs/ROADMAP.md](docs/ROADMAP.md)
- release process: [docs/RELEASE.md](docs/RELEASE.md)

## macOS Packaging

Use `topside bundle-app` to create a local `Topside.app` bundle. Prefix with `--workspace <PATH>` if you want the app to embed a default workspace, and pass `--icon` with a `.icns` file if you want a custom icon.

Use `./scripts/package-macos-release.sh` to build the release binary, generate `dist/Topside.app`, and package:

- `dist/topside-macos-<arch>.tar.gz`
- `dist/topside-macos-<arch>.dmg`
- `dist/checksums.txt`

Pass `--sign-identity` if you want the script to codesign the `.app` and `.dmg`.

## License

Dual licensed under:

- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)
