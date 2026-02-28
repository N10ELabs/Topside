# n10e Sync Roadmap

## Phase 1: Manual Repo Sync Baseline

- [x] Add a `Sync Project` action in Project Settings
- [x] Use the linked local folder as an external project source
- [x] Scan for `to-do.md`, `todo.md`, and `TOD
- [x] Parse markdown checkboxes into `n10e` tasks
- [x] Keep sync one-way from repo into `n10e`
- [x] Create imported tasks without writing back to repo files
- [x] Show last sync time and sync result summary in the UI

## Phase 2: Broader Context Import and Background Sync
- [ ] Add support for selected `docs/` content
- [ ] Surface repo file provenance in the UI
- [ ] Add background sync for linked projects
- [ ] Add task-level links back to source files
- [ ] Define stale-task handling for removed repo items

## Phase 3: Optional Bidirectional Sync

- [ ] Define conflict policy between repo files and `n10e`
- [ ] Define ownership rules for synced tasks
- [ ] Add safe write-back support for `to-do.md`
- [ ] Support manual reconcile flows for conflicting edits
- [ ] Add guardrails before enabling bidirectional sync by default
