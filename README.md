# n10e (codename: n10e-01)

Agent-native, local-first project management + knowledge hub.

`n10e` indexes markdown source-of-truth on disk into SQLite (FTS5), serves a lightweight Askama + htmx dashboard, and exposes curated MCP tools over stdio for coding agents.

## Status

V0 foundation implemented:
- Rust single binary (`n10e`)
- Commands: `init`, `serve`, `reindex`, `import`, `doctor`, `bench`
- Markdown frontmatter schemas for `task`, `project`, `note`
- SQLite migrations, FTS5 search, reverse wiki-link indexing
- Optimistic-lock writes (`expected_revision`) and archive-only delete path
- Immutable activity event logging with git context
- Dashboard with task board, note explorer, activity panel
- MCP stdio JSON-RPC tool surface (curated core)

## Install

### From source

```bash
cargo install --path .
```

### Local dev

```bash
cargo run -- init /path/to/workspace
cargo run -- --workspace /path/to/workspace serve
```

Dashboard default URL: `http://127.0.0.1:7410`

## CLI

```bash
n10e init [PATH]
n10e --workspace <PATH> serve
n10e --workspace <PATH> reindex
n10e --workspace <PATH> import <SOURCE_PATH>
n10e --workspace <PATH> doctor
n10e --workspace <PATH> bench --iterations 200 --query task
n10e --workspace <PATH> seed-bench --count 5000
n10e --workspace <PATH> mcp
```

## Workspace Layout

`n10e init` creates:

- `n10e.toml`
- `projects/`
- `tasks/`
- `notes/`
- `agents/`
- `archive/`
- `.n10e/index.sqlite`

## Config

See [examples/n10e.toml](examples/n10e.toml).

## Frontmatter Schemas

See [docs/SCHEMA.md](docs/SCHEMA.md).

## MCP

See [docs/MCP.md](docs/MCP.md).
Compatibility tracking: [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md).

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Roadmap Progress

See [docs/ROADMAP.md](docs/ROADMAP.md) for phase-by-phase implementation status and remaining V0 work.

## Performance Target

V0 target: `search_context` and `read_entity` p95 under 150ms on warm index and ~5k markdown files.

Use:

```bash
n10e --workspace <PATH> bench --iterations 500 --query task
```

Harness docs: [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## License

Dual licensed under:
- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)
