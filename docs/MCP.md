# MCP / Tool Interface

`n10e mcp` starts a dedicated stdio JSON-RPC endpoint with tool methods.
`n10e serve` and `n10e open` use the same warm daemon lifecycle as MCP-backed clients.
`n10e daemon` keeps the MCP service warm on a local Unix socket, and `n10e mcp`, `n10e open`, and `n10e serve` will auto-start that daemon when needed.

## Supported tool names

- `search_context`
- `read_entity`
- `list_tasks`
- `list_projects`
- `get_project_workspace`
- `create_project`
- `update_project`
- `create_task`
- `update_task`
- `reorder_project_tasks`
- `create_note`
- `update_note`
- `archive_entity`
- `list_recent_activity`

## Request style

Either:
- direct method call (`"method": "search_context"`), or
- `tools/call` with `{ "name": "search_context", "arguments": { ... } }`.

`initialize` and `tools/list` are also supported.
`tools/list` returns a JSON Schema `inputSchema` for each tool.
Standard MCP `tools/call` responses return `content` plus `structuredContent`.

## Concurrency and safety semantics

- Update/archive methods require `expected_revision`.
- Stale revisions return structured conflict errors.
- Every successful mutation emits an activity event.

## Stable Response Contract

Frozen as of 2026-03-01 for current tool names and payload shapes.

- Transport: `n10e mcp` accepts standard stdio MCP framing (`Content-Length` headers). It also tolerates newline-delimited JSON requests for local compatibility tests.
- `initialize` result shape:

```json
{
  "protocolVersion": "2024-11-05",
  "serverInfo": { "name": "n10e", "version": "<semver>" },
  "capabilities": {
    "tools": { "listChanged": false },
    "resources": { "subscribe": false, "listChanged": false }
  }
}
```

- `resources/list` result shape: `{ "resources": [] }`
- `resources/templates/list` result shape: `{ "resourceTemplates": [] }`
- `tools/list` result shape:

```json
{
  "tools": [
    {
      "name": "create_project",
      "description": "Create a project markdown entity",
      "inputSchema": { "type": "object", "properties": {}, "required": [] }
    }
  ]
}
```

- Direct method calls (`"method": "create_project"`, `"method": "list_projects"`, etc.) return the domain payload directly in JSON-RPC `result`.
- `tools/call` result shape:

```json
{
  "content": [
    {
      "type": "text",
      "text": "{\"id\":\"...\"}"
    }
  ],
  "structuredContent": { "id": "..." }
}
```

- `content[0].text` is the JSON serialization of `structuredContent`.
- Error shape follows JSON-RPC: `{ "code": <int>, "message": <string>, "data": <object?> }`.
- Conflict errors are stable:

```json
{
  "code": -32010,
  "message": "revision conflict",
  "data": {
    "expected_revision": "<caller-provided>",
    "current_revision": "<current-entity-revision>"
  }
}
```

- `archive_entity` accepts either `id` or `entity_id` plus `expected_revision`.
- Current direct tool method names are treated as stable unless intentionally versioned in a future release.

## Example request

```json
{"jsonrpc":"2.0","id":1,"method":"search_context","params":{"query":"indexer","limit":10}}
```
