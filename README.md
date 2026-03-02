# n10e (codename: n10e-01)

Local-first project context hub and agent memory layer.

`n10e` keeps markdown on disk as the source of truth, indexes it into SQLite (FTS5), serves a simplified three-pane knowledge hub UI, and retains a minimal MCP-compatible stdio endpoint for client compatibility.

The goal is not to become another generic agent dashboard. The goal is to make project context durable, inspectable, and fast for both humans and agents to pick up.

## Status

V0 foundation implemented:
- Rust single binary (`n10e`)
- Commands: `init`, `serve`, `open`, `bundle-app`, `reindex`, `import`, `doctor`, `bench`
- Markdown frontmatter schemas for `task`, `project`, `note`
- SQLite migrations, FTS5 search, reverse wiki-link indexing
- Optimistic-lock writes (`expected_revision`) and archive-only delete path
- Immutable activity event logging with git context
- Three-pane workspace UI: projects, inline to-do, and notes
- Linked project sources for local folders and GitHub repos
- Manual Phase 1 repo sync from linked local folders by scanning `to-do.md` checklist files into `n10e` tasks
- MCP stdio JSON-RPC compatibility endpoint (no operational tools exposed)

## Install

### From source

```bash
cargo install --path .
```

### Local dev

```bash
cargo run -- init /path/to/workspace
cargo run -- --workspace /path/to/workspace serve
cargo run -- --workspace /path/to/workspace open
cargo run -- bundle-app --output-dir ./dist
cargo run -- bundle-app --output-dir ./dist --icon /path/to/n10e.icns
./scripts/package-macos-release.sh --output-dir ./dist
```

Workspace default URL: `http://127.0.0.1:7410`

## CLI

```bash
n10e init [PATH]
n10e --workspace <PATH> serve
n10e --workspace <PATH> open
n10e bundle-app --output-dir ./dist [--icon /path/to/n10e.icns]
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

## macOS Packaging

Use `n10e bundle-app` to create a local `n10e.app` bundle. Pass `--icon` with a `.icns` file if you want the bundle to carry a custom app icon.

Use `./scripts/package-macos-release.sh` to build the release binary, generate `dist/n10e.app`, and package it into `dist/n10e-macos.dmg`. Pass `--sign-identity` if you want the script to codesign the `.app` and `.dmg`.

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Product Direction

See [docs/PRODUCT_DIRECTION.md](docs/PRODUCT_DIRECTION.md).

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
