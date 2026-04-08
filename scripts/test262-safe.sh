#!/bin/bash
# Run a single test262 subdirectory with aggressive safety limits.
#
# Usage:
#   bash scripts/test262-safe.sh built-ins/Array
#   bash scripts/test262-safe.sh built-ins/Array --verbose
#
# Safety layers:
#   1. Otter heap cap (`--max-heap-bytes`) — catchable JS RangeError
#      when a test tries to allocate past the limit.
#   2. OS virtual-memory cap (`ulimit -v`, Linux) — hard kill on any
#      runaway native growth that slips past the Otter cap.
#   3. Per-test timeout — already enforced by the runner, kept at 10s.
#
# Tune via environment variables:
#   MAX_HEAP_BYTES      Otter heap cap per test (default 256 MB)
#   ULIMIT_VIRTUAL_KB   OS virtual-memory cap in KB (default 2 GB)
#   TIMEOUT             Per-test timeout in seconds (default 10)

set -uo pipefail

if [ "$#" -lt 1 ]; then
    echo "Usage: bash scripts/test262-safe.sh <subdir> [additional test262 args]" >&2
    echo "Example: bash scripts/test262-safe.sh built-ins/Array --verbose" >&2
    exit 2
fi

SUBDIR="$1"
shift

MAX_HEAP_BYTES="${MAX_HEAP_BYTES:-268435456}"  # 256 MB
ULIMIT_VIRTUAL_KB="${ULIMIT_VIRTUAL_KB:-2097152}"  # 2 GB
TIMEOUT="${TIMEOUT:-10}"

echo "Running test262 subdir: $SUBDIR"
echo "  Heap cap:      $MAX_HEAP_BYTES bytes ($((MAX_HEAP_BYTES / 1024 / 1024)) MB, Otter-level)"
echo "  VM cap:        ${ULIMIT_VIRTUAL_KB} KB (OS ulimit -v, Linux only)"
echo "  Timeout:       ${TIMEOUT}s per test"

# Subshell so the ulimit cap only applies to this run.
# The test262 runner takes CLI flags directly (no `run` subcommand).
# Per-test timeout is in *milliseconds*, so multiply the seconds value.
TIMEOUT_MS=$((TIMEOUT * 1000))
(
    ulimit -v "$ULIMIT_VIRTUAL_KB" 2>/dev/null || true
    exec cargo run --profile test262 -p otter-test262 --bin test262 -- \
        --subdir "$SUBDIR" \
        --timeout "$TIMEOUT_MS" \
        --max-heap-bytes "$MAX_HEAP_BYTES" \
        "$@"
)
