#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${1:-$ROOT_DIR}"
OUTPUT_DIR="$ROOT_DIR/.topside-dev-app"
BINARY="$ROOT_DIR/target/debug/topside"
ICON="$ROOT_DIR/topside.icns"
APP_PATH="$OUTPUT_DIR/Topside.app"

cargo build --quiet --manifest-path "$ROOT_DIR/Cargo.toml"

"$BINARY" \
  --workspace "$WORKSPACE" \
  bundle-app \
  --output-dir "$OUTPUT_DIR" \
  --icon "$ICON" >/dev/null

exec "$APP_PATH/Contents/MacOS/topside" "$WORKSPACE"
