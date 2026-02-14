#!/bin/bash
# Run full test262 suite by subdirectory to survive VM crashes.
# Each subdirectory runs in a separate process — if one aborts, the next continues.
# Results are merged at the end and ES_CONFORMANCE.md is generated.
#
# Usage: bash scripts/test262-full-run.sh

set -uo pipefail

RESULTS_DIR="test262_results"
MERGED="$RESULTS_DIR/latest.json"
TIMEOUT=10
TEST_DIR="tests/test262/test"

mkdir -p "$RESULTS_DIR"
rm -f "$RESULTS_DIR"/batch_*.json

echo "=== Test262 Full Run (per-directory batches, timeout: ${TIMEOUT}s) ==="

# Collect top-level test subdirectories
DIRS=()

# built-ins: one batch per built-in object
for d in "$TEST_DIR"/built-ins/*/; do
    name=$(basename "$d")
    DIRS+=("built-ins/$name")
done

# language: one batch per feature area
for d in "$TEST_DIR"/language/*/; do
    name=$(basename "$d")
    DIRS+=("language/$name")
done

# annexB, staging (as single batches)
[ -d "$TEST_DIR/annexB" ] && DIRS+=("annexB")
[ -d "$TEST_DIR/staging" ] && DIRS+=("staging")

TOTAL=${#DIRS[@]}
echo "Found $TOTAL directory batches"

BATCH=0
PASSED_TOTAL=0
FAILED_TOTAL=0

for dir in "${DIRS[@]}"; do
    BATCH=$((BATCH + 1))
    BATCH_FILE="$RESULTS_DIR/batch_${BATCH}.json"

    printf "\r[%3d/%d] %-50s" "$BATCH" "$TOTAL" "$dir"

    # Run batch (may crash — that's OK, continue with next)
    cargo run -p otter-test262 --bin test262 -- run \
        --subdir "$dir" --timeout "$TIMEOUT" --save "$BATCH_FILE" \
        2>/dev/null 1>/dev/null
    EXIT_CODE=$?

    if [ "$EXIT_CODE" -eq 134 ] || [ "$EXIT_CODE" -eq 139 ] || [ "$EXIT_CODE" -eq 6 ]; then
        printf " CRASHED (signal %d)\n" "$EXIT_CODE"
    fi
done

echo ""
echo ""
echo "=== Merging batch results ==="

python3 -c "
import json, glob, os

results = []
batch_count = 0
for f in sorted(glob.glob('$RESULTS_DIR/batch_*.json')):
    try:
        with open(f) as fh:
            d = json.load(fh)
            batch_results = d.get('results', [])
            results.extend(batch_results)
            batch_count += 1
    except Exception as e:
        print(f'  Warning: Could not load {os.path.basename(f)}: {e}')

# Build merged report
passed = sum(1 for r in results if r['outcome'] == 'Pass')
failed = sum(1 for r in results if r['outcome'] == 'Fail')
skipped = sum(1 for r in results if r['outcome'] == 'Skip')
timeout = sum(1 for r in results if r['outcome'] == 'Timeout')
crashed = sum(1 for r in results if r['outcome'] == 'Crash')
total = len(results)
run_count = passed + failed + timeout + crashed
pass_rate = (passed / run_count * 100) if run_count > 0 else 0

# Build per-feature stats
by_feature = {}
for r in results:
    for feat in r.get('features', []):
        if feat not in by_feature:
            by_feature[feat] = {'total': 0, 'passed': 0, 'failed': 0, 'skipped': 0}
        by_feature[feat]['total'] += 1
        if r['outcome'] == 'Pass':
            by_feature[feat]['passed'] += 1
        elif r['outcome'] == 'Fail':
            by_feature[feat]['failed'] += 1
        elif r['outcome'] == 'Skip':
            by_feature[feat]['skipped'] += 1

merged = {
    'timestamp': '',
    'otter_version': '0.1.0',
    'test262_commit': None,
    'duration_secs': 0,
    'summary': {
        'total': total,
        'passed': passed,
        'failed': failed,
        'skipped': skipped,
        'timeout': timeout,
        'crashed': crashed,
        'pass_rate': pass_rate,
        'by_feature': by_feature,
        'failures': []
    },
    'results': results
}

with open('$MERGED', 'w') as f:
    json.dump(merged, f)

print(f'Loaded {batch_count} batch files')
print(f'Total: {total} test results')
print(f'Passed: {passed}/{run_count} ({pass_rate:.1f}%)')
print(f'Failed: {failed}, Timeout: {timeout}, Skipped: {skipped}')
"

echo ""
echo "=== Generating ES_CONFORMANCE.md ==="
cargo run -p otter-test262 --bin gen-conformance 2>&1 | grep "Generated"

echo ""
echo "Done! Results in $MERGED, conformance in ES_CONFORMANCE.md"
