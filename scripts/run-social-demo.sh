#!/bin/zsh
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE_DIR="$ROOT_DIR/examples/social-demo-template"

usage() {
  cat <<'EOF'
Usage: scripts/run-social-demo.sh [--prepare-only]

Creates a fresh temporary copy of the social demo workspace and, by default,
opens it in the native n10e desktop shell.

Options:
  --prepare-only   Create the temp workspace and print its path without launching
  -h, --help       Show this help text
EOF
}

MODE="open"
if [[ $# -gt 0 ]]; then
  case "$1" in
    --prepare-only)
      MODE="prepare"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
fi

if [[ $# -gt 0 ]]; then
  echo "unexpected extra arguments" >&2
  usage >&2
  exit 1
fi

if [[ ! -d "$TEMPLATE_DIR" ]]; then
  echo "missing demo template at $TEMPLATE_DIR" >&2
  exit 1
fi

TEMP_ROOT="${TMPDIR:-/tmp}"
TEMP_ROOT="${TEMP_ROOT%/}"
WORKSPACE_DIR="$(mktemp -d "$TEMP_ROOT/n10e-social-demo.XXXXXX")"
cp -R "$TEMPLATE_DIR/." "$WORKSPACE_DIR/"

echo "Prepared social demo workspace at $WORKSPACE_DIR"

if [[ "$MODE" == "prepare" ]]; then
  echo "Run: cargo run -- --workspace \"$WORKSPACE_DIR\" open"
  exit 0
fi

cd "$ROOT_DIR"
exec cargo run -- --workspace "$WORKSPACE_DIR" open
