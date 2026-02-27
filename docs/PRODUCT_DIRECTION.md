# Product Direction

Last updated: 2026-02-27

## Positioning

`n10e` is a local-first agent workspace and memory layer for software projects.

The product is not trying to win by becoming the loudest orchestration dashboard. The differentiation is durable project context:

- plans, tasks, and notes live as markdown on disk
- context is indexed and queryable through fast local search
- agents get a curated MCP surface for reading and mutating that context safely
- every mutation leaves an immutable activity trail with git context

The UI exists to help a human plan, inspect, and hand off work alongside agents.

## Core Promise

If a human plans a project in `n10e`, any compatible agent should be able to pick up the current state quickly, understand what matters, and continue without rebuilding context from scratch.

## Product Pillars

- Planning surface: turn intent into projects, tasks, and notes quickly.
- Shared memory: keep context durable, local, and inspectable outside any single vendor UI.
- Safe coordination: use optimistic locking, archive-only deletion, and append-only activity.
- Fast retrieval: keep search and entity reads fast enough to feel ambient during agent workflows.
- Tool neutrality: work as infrastructure underneath MCP-capable clients, not only inside one shell.

## Non-Goals

- generic cloud collaboration suite
- chat-first agent wrapper with thin state
- multi-user auth and permissions in V0
- binary asset management beyond link/reference patterns
- trying to replace IDEs, git hosts, or agent runtimes

## UI Direction

The UI should read as an operator workbench, not a generic analytics dashboard.

- foreground project intent and handoff readiness
- make active tasks, context notes, and live activity visible at once
- surface agent ownership clearly
- preserve low-latency interactions with minimal chrome
- keep the interface useful even when another tool is doing the actual code execution

## Highest-Value Next Scope

- project brief and handoff artifacts that agents can consume directly
- scoped context packs per project, branch, or task
- richer conflict payloads with concise diff context
- git/worktree awareness in activity and workspace state
- stable real-client compatibility validation across Codex and Claude Code

## Strategic Test

`n10e` succeeds if users describe it as the place where project context lives, not merely the screen where they launch agents.
