# V0 Plan Tracker (Against Original 6-Week Plan)

Last updated: 2026-02-26

## Executive Snapshot

- [x] Core architecture and end-to-end local product loop are implemented.
- [ ] Codex + Claude Code compatibility proof is complete.
- [x] 5k-file performance proof meets target (<150ms p95 read/search) on local benchmark run.
- [ ] Full integration/hardening test suite is complete.
- [x] Workspace create flows are responsive (task/note/project create paths now wired and validated).
- [x] Workspace visual pass updated to a tri-pane operator layout.
- [x] Product direction reframed around local-first planning, shared memory, and agent handoff.

## Recent Progress (2026-02-26)

- [x] Fixed unresponsive dashboard create actions by addressing invalid `project_id` handling in form submits.
- [x] Added project creation from dashboard so users can create valid project IDs before creating tasks.
- [x] Added HTTP-layer project existence validation for task and note creation/update paths (returns 400 instead of opaque 500).
- [x] Normalized blank note `project_id` to optional/none behavior.
- [x] Added local JSON route behavior and fallback UI handling for create actions.
- [x] Reworked dashboard UI toward the provided mockup style: dark shell, status strip, tri-pane layout, document-view center panel, event-log rail.
- [x] Updated dashboard stats context (`server_port`, `task_count`, `project_count`, `note_count`) and surfaced in UI.
- [x] Started Obsidian-style interaction model: project list in left rail with project-scoped workspace tabs.
- [x] Added selected-project routing (`/?project_id=...`) that scopes center-pane tasks/notes and quick-capture forms.
- [x] Added project-scoped workspace fetches (`/api/projects/{id}/workspace`) with preserved tab context.
- [x] Refined interaction model to a Workbench-style tri-column workspace: Tasks, Notes, and Recent Activity/System Status under selected project.
- [x] Added functional reindex actions (`POST /reindex`) from topbar and system status panel with workspace refresh.
- [x] Simplified task interaction to checklist-style rows with quick done toggle and archive action.

## 1) Summary Goals

- [x] Build a single Rust binary `topside`.
- [x] Deliver integrated minimal core (planning queue + note context + compact activity panel).
- [x] Retain a minimal MCP compatibility endpoint for client handshakes and discovery.
- [x] Record append-only activity events for all mutations.
- [ ] Validate minimal MCP compatibility behavior against Codex and Claude Code.
- [x] Prove p95 read/search latency under 150ms on ~5k-file warm corpus.
- [ ] Add CI gating for integration and performance acceptance.

## 2) Product Scope

- [x] Local single-user workflow.
- [x] Markdown source-of-truth for notes/tasks/projects.
- [x] Indexed search, backlinks, activity tracking.
- [x] Lightweight workspace UI, CLI, and MCP compatibility endpoint.
- [x] Keep cloud sync/auth/collab/mobile out of V0.
- [x] Keep binary file handling as link-only in V0.
- [x] Keep orchestration-dashboard competition out of scope; optimize for durable context instead.

## 3) Key Architecture Decisions

- [x] Rust stable + single-process run model.
- [x] Axum HTTP + embedded MCP runtime.
- [x] SQLite via rusqlite + FTS5.
- [x] Askama + JSON API + in-page JavaScript state controller.
- [x] Startup full scan + watcher + debounced incremental indexing.
- [x] Overflow/needs-rescan handling triggers full rescan.
- [x] `expected_revision` optimistic lock on mutable write paths.
- [x] Archive-only delete behavior (no hard delete exposed).

## 4) Workspace and File Conventions

- [x] `topside.toml` at workspace root.
- [x] `topside init` scaffolds `projects/`, `tasks/`, `notes/`, `agents/`, `archive/`.
- [x] Enforce write boundary to workspace root.
- [x] Keep `PROJECT_CODENAME = "Topside"` default.
- [x] Ship the CLI/binary as `topside`.

## 5) Public Interfaces

### CLI

- [x] `topside init`
- [x] `topside serve`
- [x] `topside reindex`
- [x] `topside import <path>`
- [x] `topside doctor`
- [x] `topside bench`
- [x] `topside seed-bench --count <n>`
- [x] `topside mcp`

### Config (`topside.toml`)

- [x] `codename`, `workspace_root`, `dirs`, `server`, `index`, `search` sections.

### Markdown Schemas

- [x] Task schema fields and enums.
- [x] Project schema fields and enums.
- [x] Note schema fields.
- [x] UTC timestamp semantics and revision hashing.

### Linking Model

- [x] Support `[[task:...]]`, `[[project:...]]`, `[[note:...]]`.
- [x] Index reverse links in `entity_links`.

### MCP Compatibility Surface

- [x] `initialize`
- [x] `ping`
- [x] `resources/list`
- [x] `resources/templates/list`
- [x] `tools/list` returns an empty tool array
- [x] `tools/call` returns `unknown tool name`
- [x] Direct tool methods return `method not found`

### HTTP/UI Surface

- [x] `GET /`
- [x] `POST /reindex`
- [x] `GET /api/projects`
- [x] `POST /api/projects`
- [x] `PATCH /api/projects/{id}`
- [x] `POST /api/projects/{id}/archive`
- [x] `POST /api/projects/{id}/sync`
- [x] `GET /api/projects/{id}/workspace`
- [x] `POST /api/tasks`
- [x] `PATCH /api/tasks/{id}`
- [x] `POST /api/tasks/{id}/archive`
- [x] `POST /api/tasks/reorder`
- [x] `POST /api/notes`
- [x] `PATCH /api/notes/{id}`
- [x] `POST /api/system/pick-directory`
- [x] `POST /api/system/open-path`
- [x] Keep auth out of V0 (trusted localhost).
- [x] Use the UI as an operator workspace for planning and context inspection, not just a mutation shell.

## 6) Data Layer Design

- [x] `schema_migrations`
- [x] `files`
- [x] `entities`
- [x] `tasks`
- [x] `projects`
- [x] `notes`
- [x] `entity_links`
- [x] `activity_events`
- [x] `fts_documents` (FTS5)
- [x] BM25-backed full-text query path.
- [x] Record git branch + commit in activity.
- [ ] Persist/show git dirty-state in activity/UI.

## 7) Runtime Flow

- [x] `serve` startup: config -> migrations -> full scan -> watcher -> HTTP/MCP.
- [x] File-change flow: debounce -> parse -> upsert -> FTS refresh.
- [x] Write flow: boundary -> schema/revision validation -> atomic markdown write -> reindex -> activity event.

## 8) Implementation Phases (6-Week Plan Tracking)

### Week 1: Foundation

- [x] Initialize Rust workspace and module layout.
- [x] Implement config parsing and `topside init`.
- [x] Add SQLite connection, migration runner, base schema.
- [x] Implement markdown parser/frontmatter validator and ULID utilities.

### Week 2: Indexer + Search

- [x] Implement startup full scan and file watcher pipeline.
- [x] Implement FTS5 indexing and `search_context`.
- [x] Add wiki-link extraction and reverse-link indexing.
- [x] Add `topside reindex` and health checks in `doctor`.

### Week 3: MCP Core

- [x] Implement MCP server with stdio transport first.
- [x] Add project/task/note read + create/update + archive tools.
- [x] Add optimistic lock error model and structured responses.
- [x] Add activity event capture and git-context enrichment.
- [x] Add automated Codex-style direct-method MCP compatibility profile tests.
- [x] Add automated Claude-style `tools/call` MCP compatibility profile tests.
- [ ] Run compatibility validation against Codex and Claude Code.

### Week 4: Workspace Core

- [x] Build Askama templates + JSON endpoints.
- [x] Deliver task planning surface + note explorer + compact activity panel.
- [x] Add workspace refresh behavior and basic actions.
- [x] Add project creation workflow in workspace UI.
- [x] Harden form behavior with project ID validation and clearer bad-request handling.
- [x] Complete mockup-inspired UI redesign pass (dark tri-pane workspace shell).
- [x] Deliver Obsidian-style project navigation baseline (left project rail + project-scoped task/note workspace).
- [x] Apply Workbench-style interaction pass while preserving existing visual theme.

### Week 5: Hardening + Import + Packaging

- [x] Add `topside import`.
- [x] Add watcher overflow-triggered full rescan path.
- [x] Add malformed-frontmatter tolerant indexing behavior (skip bad files, continue).
- [x] Add DB lock contention handling baseline (`busy_timeout`).
- [x] Prepare `cargo install` path.
- [x] Add release metadata/workflow scaffolding.
- [x] Add Homebrew formula scaffolding.

### Week 6: Test/Perf/Docs

- [x] Complete integration tests for MCP + HTTP flows.
- [x] Tune and prove latency targets on ~5k files.
- [x] Finalize architecture/MCP/schema/migration/rename docs.

## 9) Acceptance Criteria Tracker

### Functional

- [x] `init` creates expected directories/config with the Topside default codename.
- [x] Startup indexes empty and imported workspace (smoke validated).
- [x] Automated MCP CRUD integration tests cover markdown + DB + activity.
- [x] Automated conflict-path test (stale `expected_revision`) prevents overwrite.
- [x] Automated archive-path tests verify preserved queryability/state.
- [ ] Automated backlink tests for create/update/rename path scenarios.
- [x] Automated workspace mutation tests.

### Compatibility

- [x] MCP smoke tests against Codex-style request profile (simulated).
- [x] MCP smoke tests against Claude-style request profile (simulated).
- [x] MCP smoke tests against real Codex client runtime.
- [ ] MCP smoke tests against real Claude Code client runtime.
- [x] Freeze and validate stable tool response contract.
- [x] Document real-client validation blockers and current environment status in `docs/COMPATIBILITY.md`.

### Performance

- [x] Implement benchmark command.
- [x] Build/seed ~5k markdown benchmark corpus.
- [x] Capture warm-index p95 for `search_context` and `read_entity` under 150ms.
- [ ] Capture incremental reindex median under 300ms.

Benchmark evidence (2026-02-26, local run):
- [x] `seed_bench::created=5000`
- [x] `benchmark::search_p95_ms=12.875`
- [x] `benchmark::read_p95_ms=1.231`

### Reliability

- [x] Watcher overflow triggers full rescan.
- [x] Add explicit tests for corrupt frontmatter isolation.
- [x] Add explicit migration upgrade-path tests (idempotence baseline).

## 10) Distribution Plan

- [x] Primary install path via `cargo install`.
- [x] macOS release workflow scaffolding in GitHub Actions.
- [x] OSS dual license (`MIT OR Apache-2.0`).
- [ ] Publish first tagged release artifacts.
- [ ] Maintain Homebrew formula with real checksum/version.

## 11) Explicit Assumptions and Defaults

- [x] Personal single-user local usage is primary objective.
- [x] macOS is first-class target for V0.
- [x] No authentication/token gate in V0.
- [x] No cloud sync or multi-user features in V0.
- [x] No first-class binary attachment management in V0.
- [x] Codename-driven naming remains default until rebrand.
- [x] Every task belongs to one project.
- [x] Completed tasks remain in place with `done` status.
- [x] Activity history is append-only by default.

## Highest-Value Next Steps

- [ ] Expand backlink integration tests to include update/rename-path scenarios.
- [ ] Run real-client Codex and Claude Code MCP compatibility matrix and document results.
- [ ] Add direct handoff primitives: brief templates, context packs, and agent-readable summaries.
- [ ] Surface git/worktree lineage and dirty-state in the UI and activity model.
- [ ] Close remaining hardening gaps: advanced contention tests and release artifact finalization.
