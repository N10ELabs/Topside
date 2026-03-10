#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
REPO="N10ELabs/Topside"
VERSION=""
OUTPUT_PATH="$ROOT_DIR/Formula/topside.rb"

usage() {
  cat <<'EOF'
Usage: scripts/render-homebrew-formula.sh --version X.Y.Z [--repo OWNER/REPO] [--output PATH]

Downloads the GitHub source archive for a tagged release, computes the SHA-256,
and writes a Homebrew formula for Topside.

Options:
  --version X.Y.Z       Release version without the leading v (required)
  --repo OWNER/REPO     GitHub repository slug (default: N10ELabs/Topside)
  --output PATH         Destination formula path (default: ./Formula/topside.rb)
  -h, --help            Show this help text
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      [[ $# -ge 2 ]] || { echo "missing value for --version" >&2; exit 1; }
      VERSION="$2"
      shift 2
      ;;
    --repo)
      [[ $# -ge 2 ]] || { echo "missing value for --repo" >&2; exit 1; }
      REPO="$2"
      shift 2
      ;;
    --output)
      [[ $# -ge 2 ]] || { echo "missing value for --output" >&2; exit 1; }
      OUTPUT_PATH="$2"
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

if [[ -z "$VERSION" ]]; then
  echo "--version is required" >&2
  usage >&2
  exit 1
fi

if [[ "$OUTPUT_PATH" != /* ]]; then
  OUTPUT_PATH="$ROOT_DIR/$OUTPUT_PATH"
fi

archive_url="https://github.com/${REPO}/archive/refs/tags/v${VERSION}.tar.gz"
tmp_dir="$(mktemp -d)"
archive_path="$tmp_dir/topside-v${VERSION}.tar.gz"

cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

curl --retry 5 --retry-delay 2 -fsSL "$archive_url" -o "$archive_path"

if command -v sha256sum >/dev/null 2>&1; then
  archive_sha="$(sha256sum "$archive_path" | awk '{print $1}')"
else
  archive_sha="$(shasum -a 256 "$archive_path" | awk '{print $1}')"
fi

mkdir -p "$(dirname "$OUTPUT_PATH")"

cat >"$OUTPUT_PATH" <<EOF
class Topside < Formula
  desc "Agent-native local project management and knowledge hub"
  homepage "https://github.com/${REPO}"
  url "${archive_url}"
  sha256 "${archive_sha}"
  license "MIT OR Apache-2.0"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: ".")
  end

  test do
    system "#{bin}/topside", "--version"
  end
end
EOF

echo "Wrote formula to $OUTPUT_PATH"
