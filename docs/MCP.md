# MCP / Tool Interface

`n10e mcp` starts a dedicated stdio JSON-RPC endpoint with tool methods.
`n10e serve` also starts MCP stdio alongside the HTTP workspace UI.

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

## Concurrency and safety semantics

- Update/archive methods require `expected_revision`.
- Stale revisions return structured conflict errors.
- Every successful mutation emits an activity event.

## Example request

```json
{"jsonrpc":"2.0","id":1,"method":"search_context","params":{"query":"indexer","limit":10}}
```
