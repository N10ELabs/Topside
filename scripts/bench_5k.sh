#!/usr/bin/env bash
set -euo pipefail

WORKSPACE="${1:-$PWD/.bench-workspace}"
ITERATIONS="${2:-1000}"

run_n10e() {
  if command -v n10e >/dev/null 2>&1; then
    n10e "$@"
  else
    cargo run -- "$@"
  fi
}

echo "[n10e] workspace=$WORKSPACE"
echo "[n10e] iterations=$ITERATIONS"

run_n10e init "$WORKSPACE" >/dev/null
run_n10e --workspace "$WORKSPACE" seed-bench --count 5000
run_n10e --workspace "$WORKSPACE" bench --iterations "$ITERATIONS" --query benchmark-search-token
