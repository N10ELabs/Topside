#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TITLE="Codex Session Smoke"
SETUP_ONLY="false"
SMOKE_ROOT=""

usage() {
  cat <<'EOF'
Usage: ./scripts/codex-session-smoke.sh [--root DIR] [--title TITLE] [--setup-only]

Creates a disposable Topside workspace and local repo, injects a mock Codex CLI,
and launches the Topside macOS app for manual Codex session lifecycle checks.

Options:
  --root DIR      Reuse or create a specific smoke root directory.
  --title TITLE   Project title to seed in the smoke workspace.
  --setup-only    Prepare the workspace and repo, print instructions, then exit.
  --help          Show this help text.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      SMOKE_ROOT="${2:?missing path for --root}"
      shift 2
      ;;
    --title)
      TITLE="${2:?missing value for --title}"
      shift 2
      ;;
    --setup-only)
      SETUP_ONLY="true"
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This smoke harness launches the Topside macOS app and only runs on macOS." >&2
  exit 1
fi

if [[ -z "$SMOKE_ROOT" ]]; then
  TMP_BASE="${TMPDIR:-/tmp}"
  TMP_BASE="${TMP_BASE%/}"
  SMOKE_ROOT="$(mktemp -d "$TMP_BASE/topside-codex-smoke.XXXXXX")"
else
  mkdir -p "$SMOKE_ROOT"
fi

WORKSPACE="$SMOKE_ROOT/workspace"
REPO="$SMOKE_ROOT/repo"
CODEX_HOME="$SMOKE_ROOT/.codex"
MOCK_CODEX_BIN="$SMOKE_ROOT/mock-codex.sh"
CHECKLIST_PATH="$SMOKE_ROOT/CODEX_SESSION_SMOKE_CHECKLIST.md"

mkdir -p "$WORKSPACE" "$REPO" "$CODEX_HOME/sessions"

cat >"$MOCK_CODEX_BIN" <<'EOF'
#!/bin/sh
set -eu

cwd=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-C" ]; then
    cwd="$arg"
  fi
  prev="$arg"
done

if [ -z "$cwd" ]; then
  cwd="$(pwd)"
fi

ts="$(date -u +"%Y-%m-%dT%H:%M:%S+00:00")"
session_id="$(uuidgen | tr 'A-Z' 'a-z')"
session_dir="${TOPSIDE_CODEX_HOME}/sessions/$(date -u +%Y/%m/%d)"
mkdir -p "$session_dir"

printf '{"id":"%s","thread_name":"Topside Smoke Session","updated_at":"%s"}\n' "$session_id" "$ts" >> "${TOPSIDE_CODEX_HOME}/session_index.jsonl"
printf '{"type":"session_meta","timestamp":"%s","payload":{"id":"%s","cwd":"%s","timestamp":"%s"}}\n' "$ts" "$session_id" "$cwd" "$ts" > "$session_dir/$session_id.jsonl"
printf 'topside-smoke:%s\r\n' "$session_id"

cat
EOF
chmod +x "$MOCK_CODEX_BIN"

cat >"$CHECKLIST_PATH" <<EOF
# Codex Session Smoke Checklist

Workspace: $WORKSPACE
Repo: $REPO

1. Open the seeded "$TITLE" project and switch the right pane to Codex.
2. Click New Session and confirm a live session opens with terminal output starting with \`topside-smoke:\`.
3. Type a short prompt like \`ping\` and confirm the terminal echoes it back.
4. Create a second session from the same project.
5. Archive the second session and confirm Topside falls back to the remaining session instead of showing a stale selection.
6. Create another session, then click End Session and confirm it becomes resumable.
7. Resume that resumable session and confirm the terminal reconnects.
8. Archive the resumed session and confirm it disappears from the session rail.
9. Repeat steps 2-8 a second time to catch stale terminal/socket state after repeated churn.

Cleanup:
\`rm -rf "$SMOKE_ROOT"\`
EOF

cargo build --quiet --manifest-path "$ROOT_DIR/Cargo.toml" --bin topside --bin topside-smoke-codex-setup
"$ROOT_DIR/target/debug/topside-smoke-codex-setup" \
  --workspace "$WORKSPACE" \
  --repo "$REPO" \
  --title "$TITLE"

export TOPSIDE_CODEX_BIN="$MOCK_CODEX_BIN"
export TOPSIDE_CODEX_HOME="$CODEX_HOME"

cat <<EOF

Topside Codex session smoke environment is ready.

Workspace: $WORKSPACE
Repo: $REPO
Mock Codex bin: $MOCK_CODEX_BIN
Mock Codex home: $CODEX_HOME
Checklist: $CHECKLIST_PATH

Manual flow:
1. Open the seeded "$TITLE" project.
2. Run through the checklist in $CHECKLIST_PATH.
3. When finished, remove the smoke root with:
   rm -rf "$SMOKE_ROOT"
EOF

if [[ "$SETUP_ONLY" == "true" ]]; then
  exit 0
fi

exec "$ROOT_DIR/scripts/open-topside-app.sh" "$WORKSPACE"
