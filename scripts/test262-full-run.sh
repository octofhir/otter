#!/bin/bash
# Run full test262 suite by subdirectory to survive VM crashes.
# Each subdirectory runs in a separate process — if one aborts, the next continues.
# Results are merged at the end and ES_CONFORMANCE.md is generated.
#
# Usage:
#   bash scripts/test262-full-run.sh
#
# Environment overrides:
#   TIMEOUT=10                  # seconds per test
#   MAX_HEAP_BYTES=536870912    # Otter per-test heap cap (512 MB default)
#   ULIMIT_VIRTUAL_KB=4194304   # OS virtual-memory cap in KB (4 GB default, Linux only)
#   RESULTS_DIR=test262_results # where to write batch/merged JSON
#
# Safety: each child process gets two independent memory caps:
#   - Inner cap: --max-heap-bytes (Otter heap limit, catchable RangeError)
#   - Outer cap: ulimit -v (OS virtual-memory guard, process kill, Linux)
# Array subdirs that previously OOM-ed the host should now finish as either
# `OutOfMemory` outcomes or, worst case, a SIGKILL the batch script survives.

set -uo pipefail

RESULTS_DIR="${RESULTS_DIR:-test262_results}"
MERGED="$RESULTS_DIR/latest.json"
TIMEOUT="${TIMEOUT:-10}"
TEST_DIR="tests/test262/test"
MAX_HEAP_BYTES="${MAX_HEAP_BYTES:-536870912}"
ULIMIT_VIRTUAL_KB="${ULIMIT_VIRTUAL_KB:-4194304}"
TIMEOUT_MS=$((TIMEOUT * 1000))

mkdir -p "$RESULTS_DIR"
rm -f "$RESULTS_DIR"/batch_*.json "$RESULTS_DIR"/batch_*.log

echo "=== Test262 Full Run ==="
echo "  Heap cap:  $MAX_HEAP_BYTES bytes ($((MAX_HEAP_BYTES / 1024 / 1024)) MB, Otter-level)"
echo "  VM cap:    ${ULIMIT_VIRTUAL_KB} KB (ulimit -v, Linux only)"
echo "  Timeout:   ${TIMEOUT}s per test"
echo "  Results:   $RESULTS_DIR"
echo ""

# Pre-build the binaries once so per-batch iterations don't rebuild.
echo "=== Building runner + merger ==="
cargo build --profile test262 -p otter-test262 --bin test262 --bin merge-reports || {
    echo "Build failed" >&2
    exit 1
}

TEST262_BIN="target/test262/test262"
MERGE_BIN="target/test262/merge-reports"

# Collect top-level test subdirectories.
DIRS=()
for d in "$TEST_DIR"/built-ins/*/; do
    name=$(basename "$d")
    DIRS+=("built-ins/$name")
done
for d in "$TEST_DIR"/language/*/; do
    name=$(basename "$d")
    DIRS+=("language/$name")
done
[ -d "$TEST_DIR/annexB" ] && DIRS+=("annexB")
[ -d "$TEST_DIR/staging" ] && DIRS+=("staging")

TOTAL=${#DIRS[@]}
echo "Found $TOTAL directory batches"
echo ""

BATCH=0
FAILED_BATCHES=0

for dir in "${DIRS[@]}"; do
    BATCH=$((BATCH + 1))
    BATCH_FILE="$RESULTS_DIR/batch_$(printf '%04d' "$BATCH").json"
    LOG_FILE="$RESULTS_DIR/batch_$(printf '%04d' "$BATCH").log"

    printf "[%3d/%d] %-50s" "$BATCH" "$TOTAL" "$dir"

    # Run batch inside a subshell so the ulimit cap only affects this child.
    # `ulimit -v` is Linux-only — macOS silently ignores it.
    (
        ulimit -v "$ULIMIT_VIRTUAL_KB" 2>/dev/null || true
        "$TEST262_BIN" \
            --subdir "$dir" \
            --timeout "$TIMEOUT_MS" \
            --max-heap-bytes "$MAX_HEAP_BYTES" \
            --save "$BATCH_FILE"
    ) >"$LOG_FILE" 2>&1
    EXIT_CODE=$?

    case "$EXIT_CODE" in
        0)
            # Brief per-batch pass-rate from the saved report (best effort).
            if [ -f "$BATCH_FILE" ]; then
                PASSED=$(grep -o '"passed":[[:space:]]*[0-9]*' "$BATCH_FILE" | head -1 | grep -o '[0-9]*' || echo "?")
                FAILED=$(grep -o '"failed":[[:space:]]*[0-9]*' "$BATCH_FILE" | head -1 | grep -o '[0-9]*' || echo "?")
                printf " ok (%s pass, %s fail)\n" "$PASSED" "$FAILED"
            else
                printf " ok (no report — check %s)\n" "$LOG_FILE"
            fi
            ;;
        134|139|6)
            printf " CRASHED (signal %d, see %s)\n" "$EXIT_CODE" "$LOG_FILE"
            FAILED_BATCHES=$((FAILED_BATCHES + 1))
            ;;
        137)
            printf " KILLED (SIGKILL — likely OOM/ulimit, see %s)\n" "$LOG_FILE"
            FAILED_BATCHES=$((FAILED_BATCHES + 1))
            ;;
        *)
            printf " exit %d (see %s)\n" "$EXIT_CODE" "$LOG_FILE"
            FAILED_BATCHES=$((FAILED_BATCHES + 1))
            ;;
    esac
done

echo ""
echo "=== Merging batch results ==="
"$MERGE_BIN" --input "$RESULTS_DIR/batch_*.json" --output "$MERGED" --verbose || {
    echo "Merge failed" >&2
    exit 1
}

echo ""
echo "=== Generating ES_CONFORMANCE.md ==="
cargo run --profile test262 -p otter-test262 --bin gen-conformance 2>&1 | grep -E "Generated|Error" || true

echo ""
if [ "$FAILED_BATCHES" -gt 0 ]; then
    echo "Done (with $FAILED_BATCHES crashed batch(es)). Results in $MERGED, conformance in ES_CONFORMANCE.md"
else
    echo "Done. Results in $MERGED, conformance in ES_CONFORMANCE.md"
fi
