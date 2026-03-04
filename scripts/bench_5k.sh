#!/usr/bin/env bash
set -euo pipefail

WORKSPACE="${1:-$PWD/.bench-workspace}"
ITERATIONS="${2:-1000}"

run_topside() {
  if command -v topside >/dev/null 2>&1; then
    topside "$@"
  else
    cargo run -- "$@"
  fi
}

echo "[topside] workspace=$WORKSPACE"
echo "[topside] iterations=$ITERATIONS"

run_topside init "$WORKSPACE" >/dev/null
run_topside --workspace "$WORKSPACE" seed-bench --count 5000
run_topside --workspace "$WORKSPACE" bench --iterations "$ITERATIONS" --query benchmark-search-token
