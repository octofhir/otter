#!/usr/bin/env bash

set -euo pipefail

BENCHMARKS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$BENCHMARKS_DIR/.." && pwd)"
CACHE_DIR="${OTTER_BENCH_CACHE:-$BENCHMARKS_DIR/.suite-cache}"
RESULTS_DIR="$BENCHMARKS_DIR/results"

mkdir -p "$CACHE_DIR" "$RESULTS_DIR"

ensure_otter_bin() {
  if [ -n "${OTTER_BIN:-}" ]; then
    if [ ! -x "$OTTER_BIN" ]; then
      echo "error: OTTER_BIN is not executable: $OTTER_BIN" >&2
      exit 1
    fi
    printf '%s\n' "$OTTER_BIN"
    return
  fi

  cargo build --release -q -p otter-cli
  local bin="$ROOT/target/release/otter"
  if [ -x "$bin.exe" ]; then
    bin="$bin.exe"
  fi
  if [ ! -x "$bin" ]; then
    echo "error: release otter binary not found after build: $bin" >&2
    exit 1
  fi
  printf '%s\n' "$bin"
}

timestamp() {
  date -u '+%Y%m%dT%H%M%SZ'
}

run_otter_files() {
  local bin="$1"
  shift
  local timeout="${OTTER_BENCH_TIMEOUT:-0}"
  OTTER_JIT="${OTTER_JIT:-1}" "$bin" --timeout "$timeout" run "$@"
}

run_external_file() {
  local runtime="$1"
  local file="$2"
  if ! command -v "$runtime" >/dev/null 2>&1; then
    echo "skip: $runtime not found" >&2
    return 0
  fi
  case "$runtime" in
    node) node "$file" ;;
    bun) bun "$file" ;;
    *) echo "error: unsupported external runtime: $runtime" >&2; return 2 ;;
  esac
}
