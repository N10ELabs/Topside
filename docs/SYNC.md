# Sync Model

Last updated: 2026-03-08

Topside currently supports multiple repo-adjacent sync paths. They are related, but they are not the same mechanism.

## Project Source Types

Projects can be linked to:

- a local folder
- a GitHub repo URL

Only local-folder projects support live file-backed sync and live Codex session launches. GitHub-linked projects currently act as source metadata, not as full local workspaces.

## Managed Task Sync

Managed task sync is the main bidirectional task workflow for local projects.

- default managed task file: `docs/to-do.md`
- default sidecar file: `docs/.to-do.topside-sync.json`
- project state tracks whether sync is enabled and whether it is `live`, `paused`, or `conflict`

What it does:

- writes Topside task state out to the managed markdown file
- preserves task identity in the sidecar instead of polluting the visible markdown
- watches the managed file for external edits and imports them back into Topside

Conflict behavior:

- if both sides change before reconciliation, Topside records a conflict instead of silently overwriting
- conflict resolution can choose the file as source of truth or the current Topside state as source of truth

Important workspace note:

- in this repository, `docs/to-do.md` is an actively synced workspace file and should be maintained rather than deleted or renamed casually

## Manual Project Scan

Topside also supports a one-shot scan of linked local folders for checklist files:

- `to-do.md`
- `todo.md`
- `TODO.md`

This scan imports checkbox items as Topside tasks. It is useful for intake and reconciliation, but it is separate from the managed live sync loop above.

## Linked Note Sync

Local linked projects can also sync repo markdown files into Topside notes.

Common pattern:

- project docs live under `docs/`
- Topside links a repo markdown file to a note record
- the file becomes the note's sync target

Topside tracks:

- `sync_kind`
- `sync_path`
- `sync_status`
- file hash and inbound/outbound timestamps
- conflict summary and time when a note hits conflict

Conflict behavior is explicit. Topside does not silently choose between local and file changes when both changed first.

## What Is Not Synced

- arbitrary binary assets
- remote GitHub repository contents without a local linked folder
- a broad MCP tool surface

## Related Docs

- [PROJECT.md](PROJECT.md)
- [SCHEMA.md](SCHEMA.md)
- [README.md](../README.md)
