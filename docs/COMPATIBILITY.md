# MCP Compatibility Matrix

Last updated: 2026-03-01

## Current Validation Levels

- `Automated profile simulation`: done
- `Real client smoke test (Codex)`: done
- `Real client smoke test (Claude Code)`: pending
- `Stable response contract`: frozen

## Automated Profiles (Implemented)

Integration tests in `tests/mcp_compat_integration.rs` validate two request styles:

1. Codex-style direct tool methods
- direct method calls like `create_project`, `create_task`, `update_task`, `search_context`
- optimistic-lock conflict behavior (`-32010`) validated

2. Claude-style `tools/call`
- `tools/list` discovery
- `tools/call` execution for `create_project`, `create_note`, `read_entity`, `list_recent_activity`

Run:

```bash
cargo test --test mcp_compat_integration
```

## Real Client Matrix (Manual, Pending)

Current environment note (2026-03-01):
- live `codex exec` smoke validation succeeded after switching MCP stdout to protocol-only output and keeping tracing on stderr.
- in `codex exec`, the agent did not get a directly callable raw `tools/list` method; instead the configured server tools were exposed in the session tool registry (`mcp__n10e-smoke__*`), while MCP resource/template discovery remained empty.
- stale `update_task` validation returned the expected structured conflict path in the real Codex client: code `-32010`, message `revision conflict`, with `expected_revision` and `current_revision` included.
- no `claude` / `claude-code` CLI binary was available locally for direct runtime smoke execution.

### Codex

- [x] configure Codex MCP to launch `n10e --workspace <path> mcp`
- [x] validate real client startup / initialize handshake
- [x] create project via configured MCP tool (`create_project`)
- [x] verify project visibility via configured MCP tool (`list_projects`)
- [x] verify workspace read via configured MCP tool (`get_project_workspace`)
- [x] verify conflict payload handling

### Claude Code

- [ ] configure Claude Code MCP to launch `n10e --workspace <path> mcp`
- [ ] run `tools/list` and `tools/call` flow
- [ ] validate task/note create+update+activity path
- [ ] verify conflict payload handling
