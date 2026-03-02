# MCP Compatibility Matrix

Last updated: 2026-03-02

## Current Validation Levels

- `Automated profile simulation`: done
- `Real client smoke test (Codex)`: protocol-only
- `Real client smoke test (Claude Code)`: pending
- `Stable response contract`: reduced

## Automated Profiles (Implemented)

Integration tests in `tests/mcp_compat_integration.rs` validate:

1. Line-delimited JSON requests
- `initialize`
- empty `tools/list`
- empty `resources/list` and `resources/templates/list`

2. Framed MCP requests
- `initialize`
- `ping`
- `tools/call` rejection for non-existent tools

Run:

```bash
cargo test --test mcp_compat_integration
```

## Current Product Direction

The MCP endpoint is intentionally kept as a minimal compatibility surface. It preserves the handshake and discovery protocol shape for clients that expect an MCP server, but it no longer exposes operational read/write tools. Runtime project interaction is expected to happen through synced markdown files.
