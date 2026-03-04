#!/bin/zsh
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS packaging is only supported on Darwin hosts" >&2
  exit 1
fi

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="$ROOT_DIR/dist"
WORKSPACE=""
ICON=""
SIGN_IDENTITY=""

usage() {
  cat <<'EOF'
Usage: scripts/package-macos-release.sh [--output-dir DIR] [--workspace PATH] [--icon FILE.icns] [--sign-identity NAME]

Builds the release binary, creates a macOS app bundle via `topside bundle-app`,
and packages the bundle into a compressed .dmg.

Options:
  --output-dir DIR       Destination directory (default: ./dist)
  --workspace PATH       Embed a default workspace path into the bundle launcher
  --icon FILE.icns       Copy a macOS .icns file into the bundle and set it as the app icon
  --sign-identity NAME   Optional codesign identity for the .app and .dmg
  -h, --help             Show this help text
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output-dir)
      [[ $# -ge 2 ]] || { echo "missing value for --output-dir" >&2; exit 1; }
      OUTPUT_DIR="$2"
      shift 2
      ;;
    --workspace)
      [[ $# -ge 2 ]] || { echo "missing value for --workspace" >&2; exit 1; }
      WORKSPACE="$2"
      shift 2
      ;;
    --icon)
      [[ $# -ge 2 ]] || { echo "missing value for --icon" >&2; exit 1; }
      ICON="$2"
      shift 2
      ;;
    --sign-identity)
      [[ $# -ge 2 ]] || { echo "missing value for --sign-identity" >&2; exit 1; }
      SIGN_IDENTITY="$2"
      shift 2
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
done

if [[ "$OUTPUT_DIR" != /* ]]; then
  OUTPUT_DIR="$ROOT_DIR/$OUTPUT_DIR"
fi

mkdir -p "$OUTPUT_DIR"

cd "$ROOT_DIR"
cargo build --release

BUNDLE_CMD=("$ROOT_DIR/target/release/topside")
if [[ -n "$WORKSPACE" ]]; then
  BUNDLE_CMD+=("--workspace" "$WORKSPACE")
fi
BUNDLE_CMD+=("bundle-app" "--output-dir" "$OUTPUT_DIR")
if [[ -n "$ICON" ]]; then
  BUNDLE_CMD+=("--icon" "$ICON")
fi

"${BUNDLE_CMD[@]}"

APP_BUNDLE="$OUTPUT_DIR/Topside.app"
DMG_PATH="$OUTPUT_DIR/topside-macos.dmg"

if [[ -n "$SIGN_IDENTITY" ]]; then
  codesign --force --deep --options runtime --sign "$SIGN_IDENTITY" "$APP_BUNDLE"
fi

rm -f "$DMG_PATH"
hdiutil create -volname "Topside" -srcfolder "$APP_BUNDLE" -ov -format UDZO "$DMG_PATH"

if [[ -n "$SIGN_IDENTITY" ]]; then
  codesign --force --sign "$SIGN_IDENTITY" "$DMG_PATH"
fi

echo "Created macOS app bundle at $APP_BUNDLE"
echo "Created macOS disk image at $DMG_PATH"
