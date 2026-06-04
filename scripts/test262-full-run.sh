#!/bin/bash
# Run the full test262 suite in per-directory batches so a native crash
# (SIGABRT/SIGSEGV/SIGKILL) in one subtree doesn't take down the whole
# run. Crashed batches are split one directory level deeper and retried
# so only the crashing subtree's results are lost. Results are merged
# into one baseline JSON + Markdown + HTML dashboard at the end.
#
# Usage:
#   bash scripts/test262-full-run.sh
#
# Environment overrides:
#   TIMEOUT=10                  # seconds per test (runner hard-caps at 30)
#   MAX_HEAP_BYTES=536870912    # Otter per-test heap cap (512 MB default)
#   ULIMIT_VIRTUAL_KB=4194304   # OS virtual-memory cap in KB (4 GB default, Linux only)
#   RESULTS_DIR=test262_results # where to write batch/merged JSON
#
# Safety: each child process gets two independent memory caps:
#   - Inner cap: --max-heap-bytes (Otter heap limit, catchable RangeError)
#   - Outer cap: ulimit -v (OS virtual-memory guard, process kill, Linux)
# built-ins/Array contains tests that would otherwise allocate tens of GB.
#
# Filters are anchored with a leading `^` (prefix match in the runner)
# so directory shards stay disjoint — `built-ins/RegExp/` must not also
# match `annexB/built-ins/RegExp/...`.

set -uo pipefail

RESULTS_DIR="${RESULTS_DIR:-test262_results}"
MERGED="$RESULTS_DIR/latest.json"
TIMEOUT="${TIMEOUT:-10}"
TEST_DIR="vendor/test262/test"
MAX_HEAP_BYTES="${MAX_HEAP_BYTES:-536870912}"
ULIMIT_VIRTUAL_KB="${ULIMIT_VIRTUAL_KB:-4194304}"
TIMEOUT_MS=$((TIMEOUT * 1000))

mkdir -p "$RESULTS_DIR"
rm -f "$RESULTS_DIR"/batch_*.json "$RESULTS_DIR"/batch_*.md "$RESULTS_DIR"/batch_*.log

echo "=== Test262 Full Run ==="
echo "  Heap cap:  $MAX_HEAP_BYTES bytes ($((MAX_HEAP_BYTES / 1024 / 1024)) MB, Otter-level)"
echo "  VM cap:    ${ULIMIT_VIRTUAL_KB} KB (ulimit -v, Linux only)"
echo "  Timeout:   ${TIMEOUT}s per test"
echo "  Results:   $RESULTS_DIR"
echo ""

echo "=== Building runner ==="
cargo build --release -p otter-test262 --bin otter-test262 || {
    echo "Build failed" >&2
    exit 1
}

TEST262_BIN="target/release/otter-test262"

# Collect top-level test subdirectories.
DIRS=()
for d in "$TEST_DIR"/built-ins/*/; do
    name=$(basename "$d")
    DIRS+=("built-ins/$name/")
done
for d in "$TEST_DIR"/language/*/; do
    name=$(basename "$d")
    DIRS+=("language/$name/")
done
[ -d "$TEST_DIR/annexB" ] && DIRS+=("annexB/")
[ -d "$TEST_DIR/staging" ] && DIRS+=("staging/")
[ -d "$TEST_DIR/intl402" ] && DIRS+=("intl402/")

TOTAL=${#DIRS[@]}
echo "Found $TOTAL directory batches"
echo ""

FAILED_BATCHES=0

# Run one anchored filter into one batch file. Args: dir-prefix, stem.
# Echoes the child's exit code.
run_filter() {
    local prefix="$1" stem="$2"
    local batch_file="$RESULTS_DIR/$stem.json"
    local log_file="$RESULTS_DIR/$stem.log"
    (
        ulimit -v "$ULIMIT_VIRTUAL_KB" 2>/dev/null || true
        "$TEST262_BIN" run \
            --filter "^$prefix" \
            --timeout "$TIMEOUT_MS" \
            --max-heap-bytes "$MAX_HEAP_BYTES" \
            --output "$batch_file"
    ) >"$log_file" 2>&1
}

describe_result() {
    local exit_code="$1" stem="$2"
    local batch_file="$RESULTS_DIR/$stem.json"
    local log_file="$RESULTS_DIR/$stem.log"
    case "$exit_code" in
        0|1)
            # Exit 1 = ran to completion with failing tests; report is valid.
            if [ -f "$batch_file" ]; then
                local passed failed
                passed=$(grep -o '"passed":[[:space:]]*[0-9]*' "$batch_file" | head -1 | grep -o '[0-9]*$' || echo "?")
                failed=$(grep -o '"failed":[[:space:]]*[0-9]*' "$batch_file" | head -1 | grep -o '[0-9]*$' || echo "?")
                printf " ok (%s pass, %s fail)\n" "$passed" "$failed"
            else
                printf " ok (no report — check %s)\n" "$log_file"
            fi
            ;;
        134|139|6)  printf " CRASHED (signal exit %d, see %s)\n" "$exit_code" "$log_file" ;;
        137)        printf " KILLED (SIGKILL — likely OOM/ulimit, see %s)\n" "$log_file" ;;
        *)          printf " exit %d (see %s)\n" "$exit_code" "$log_file" ;;
    esac
}

is_crash_exit() {
    case "$1" in
        134|139|6|137) return 0 ;;
        *) return 1 ;;
    esac
}

BATCH=0
for dir in "${DIRS[@]}"; do
    BATCH=$((BATCH + 1))
    STEM="batch_$(printf '%04d' "$BATCH")"

    printf "[%3d/%d] %-50s" "$BATCH" "$TOTAL" "$dir"
    run_filter "$dir" "$STEM"
    EXIT_CODE=$?
    describe_result "$EXIT_CODE" "$STEM"

    if is_crash_exit "$EXIT_CODE"; then
        # The whole batch report is lost on a native crash. Split one
        # directory level deeper and retry so only the crashing
        # subtree's results stay missing.
        rm -f "$RESULTS_DIR/$STEM.json"
        SUB=0
        # Subdirectories first...
        for sub in "$TEST_DIR/$dir"*/; do
            [ -d "$sub" ] || continue
            SUB=$((SUB + 1))
            subrel="${sub#"$TEST_DIR"/}"
            SUBSTEM="${STEM}_$(printf '%02d' "$SUB")"
            printf "        ↳ %-48s" "$subrel"
            run_filter "$subrel" "$SUBSTEM"
            SUB_EXIT=$?
            describe_result "$SUB_EXIT" "$SUBSTEM"
            if is_crash_exit "$SUB_EXIT"; then
                rm -f "$RESULTS_DIR/$SUBSTEM.json"
                FAILED_BATCHES=$((FAILED_BATCHES + 1))
            fi
        done
        # ...then loose files directly in the batch directory.
        for f in "$TEST_DIR/$dir"*.js; do
            [ -f "$f" ] || continue
            case "$f" in *_FIXTURE.js) continue ;; esac
            SUB=$((SUB + 1))
            subrel="${f#"$TEST_DIR"/}"
            SUBSTEM="${STEM}_$(printf '%02d' "$SUB")"
            printf "        ↳ %-48s" "$subrel"
            run_filter "$subrel" "$SUBSTEM"
            SUB_EXIT=$?
            describe_result "$SUB_EXIT" "$SUBSTEM"
            if is_crash_exit "$SUB_EXIT"; then
                rm -f "$RESULTS_DIR/$SUBSTEM.json"
                FAILED_BATCHES=$((FAILED_BATCHES + 1))
            fi
        done
    fi
done

echo ""
echo "=== Merging batch results ==="
"$TEST262_BIN" merge "$RESULTS_DIR"/batch_*.json --output "$MERGED" || {
    echo "Merge failed" >&2
    exit 1
}

echo ""
echo "=== Generating HTML dashboard ==="
"$TEST262_BIN" site "$MERGED" --output "$RESULTS_DIR/site/index.html" || {
    echo "Site generation failed" >&2
}
# Keep the contributor-book copy in sync so the published doc site
# always carries the latest dashboard (mdBook ships it verbatim).
if [ -d docs/book/src/conformance ] && [ -f "$RESULTS_DIR/site/index.html" ]; then
    cp "$RESULTS_DIR/site/index.html" docs/book/src/conformance/index.html
    echo "Dashboard copied to docs/book/src/conformance/index.html"
fi

echo ""
if [ "$FAILED_BATCHES" -gt 0 ]; then
    echo "Done (with $FAILED_BATCHES crashed sub-batch(es) — their results are missing). Results in $MERGED, dashboard in $RESULTS_DIR/site/index.html"
else
    echo "Done. Results in $MERGED, dashboard in $RESULTS_DIR/site/index.html"
fi
