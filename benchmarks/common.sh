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

# Run a command once and print "<wall_ms> <peak_rss_kb>".
#
# `/usr/bin/time` is the only peak-RSS source available without a profiler, and
# its two dialects disagree: BSD/macOS `-l` reports seconds plus bytes, GNU `-v`
# reports `h:mm:ss` plus kilobytes. Both numbers come from `time` itself rather
# than from a shell-level clock, so the wrapper's own fork/exec is excluded.
measure_wall_and_rss() {
  local stats
  stats="$(mktemp)"
  local dialect=bsd
  if ! /usr/bin/time -l "$@" >/dev/null 2>"$stats"; then
    dialect=gnu
    if ! /usr/bin/time -v "$@" >/dev/null 2>"$stats"; then
      rm -f "$stats"
      return 1
    fi
  fi
  local parsed
  parsed="$(awk -v dialect="$dialect" '
    dialect == "bsd" && $2 == "real" { ms = int($1 * 1000 + 0.5) }
    dialect == "bsd" && /maximum resident set size/ { rss = int($1 / 1024) }
    dialect == "gnu" && /Elapsed \(wall clock\) time/ {
      n = split($NF, parts, ":")
      seconds = parts[n]
      if (n > 1) { seconds += parts[n - 1] * 60 }
      if (n > 2) { seconds += parts[n - 2] * 3600 }
      ms = int(seconds * 1000 + 0.5)
    }
    dialect == "gnu" && /Maximum resident set size/ { rss = int($NF) }
    END { if (rss != "") { print ms, rss } }
  ' "$stats")"
  rm -f "$stats"
  if [ -z "$parsed" ]; then
    return 1
  fi
  printf '%s\n' "$parsed"
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
