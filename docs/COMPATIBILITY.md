# MCP Compatibility Matrix

Last updated: 2026-02-26

## Current Validation Levels

- `Automated profile simulation`: done
- `Real client smoke test (Codex)`: pending
- `Real client smoke test (Claude Code)`: pending

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

Current environment note (2026-02-26):
- attempted a live `codex exec` MCP smoke invocation, but the run did not complete in this environment and was terminated.
- no `claude` / `claude-code` CLI binary was available locally for direct runtime smoke execution.

### Codex

- [ ] configure Codex MCP to launch `n10e --workspace <path> mcp`
- [ ] run `tools/list`
- [ ] create/read/update/archive entity roundtrip
- [ ] verify conflict payload handling

### Claude Code

- [ ] configure Claude Code MCP to launch `n10e --workspace <path> mcp`
- [ ] run `tools/list` and `tools/call` flow
- [ ] validate task/note create+update+activity path
- [ ] verify conflict payload handling
