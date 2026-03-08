# MCP / Tool Interface

`topside mcp` starts a dedicated stdio JSON-RPC endpoint for protocol compatibility.

## Current Scope

- Supported protocol methods: `initialize`, `ping`, `resources/list`, `resources/templates/list`, `tools/list`
- `tools/list` returns an empty tool array
- `tools/call` is accepted but returns `unknown tool name`
- All direct tool methods return `method not found`

Operational reads and writes happen through the workspace, sync flows, and the desktop Codex integration rather than through MCP tool calls.

## Stable Response Contract

- Transport: `topside mcp` accepts standard stdio MCP framing (`Content-Length` headers). It also tolerates newline-delimited JSON requests for local compatibility tests.
- `initialize` result shape:

```json
{
  "protocolVersion": "2024-11-05",
  "serverInfo": { "name": "Topside", "version": "<semver>" },
  "capabilities": {
    "tools": { "listChanged": false },
    "resources": { "subscribe": false, "listChanged": false }
  }
}
```

- `resources/list` result shape: `{ "resources": [] }`
- `resources/templates/list` result shape: `{ "resourceTemplates": [] }`
- `tools/list` result shape: `{ "tools": [] }`
- Error shape follows JSON-RPC: `{ "code": <int>, "message": <string>, "data": <object?> }`

## Example request

```json
{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}
```
