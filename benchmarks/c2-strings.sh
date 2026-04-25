#!/bin/bash
#
# C2 String Hierarchy benchmark runner.
#
# Drives `cargo bench -p otter-vm --bench c2_string_bench` (Criterion 0.7)
# which exercises the heap-level string subsystem after the C2 rope
# refactor:
#   - lazy `+` (Cons) for `+= loop` (256 KB target → ms range vs minutes pre-C2)
#   - lazy slice (Sliced) — non-observed vs flatten-on-read paths
#   - Latin-1 storage (1 MB ASCII alloc — half the bytes of pre-Phase-4)
#   - indexOf on 256 KB haystack
#   - FNV-1a hash compute + cached read
#
# Usage:
#   ./benchmarks/c2-strings.sh                # full suite
#   ./benchmarks/c2-strings.sh concat_loop    # filter by name
#   ./benchmarks/c2-strings.sh --quick        # short measurement (10 s/case)
#   ./benchmarks/c2-strings.sh --baseline foo # save as baseline 'foo'
#   ./benchmarks/c2-strings.sh --compare foo  # compare against baseline 'foo'
#
# Output: Criterion HTML report at target/criterion/report/index.html

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Colors.
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

cd "$PROJECT_ROOT"

QUICK=0
FILTER=""
EXTRA_ARGS=()

while [ $# -gt 0 ]; do
    case "$1" in
        --quick)
            QUICK=1
            shift
            ;;
        --baseline)
            EXTRA_ARGS+=(--save-baseline "$2")
            shift 2
            ;;
        --compare)
            EXTRA_ARGS+=(--baseline "$2")
            shift 2
            ;;
        --help|-h)
            sed -n '3,20p' "$0"
            exit 0
            ;;
        *)
            FILTER="$1"
            shift
            ;;
    esac
done

if [ $QUICK -eq 1 ]; then
    EXTRA_ARGS+=(--measurement-time 10 --warm-up-time 2 --sample-size 30)
fi

echo -e "${BLUE}================================================================${NC}"
echo -e "${BLUE} C2 String Hierarchy benchmarks (Criterion 0.7)${NC}"
echo -e "${BLUE}================================================================${NC}"
echo ""
echo -e "${YELLOW}Building bench binary (release)...${NC}"
cargo bench -p otter-vm --bench c2_string_bench --no-run 2>&1 | tail -3

echo ""
echo -e "${YELLOW}Running benchmarks...${NC}"
if [ -n "$FILTER" ]; then
    echo "  filter: $FILTER"
fi
if [ ${#EXTRA_ARGS[@]} -gt 0 ]; then
    echo "  extra:  ${EXTRA_ARGS[*]}"
fi
echo ""

cargo bench -p otter-vm --bench c2_string_bench -- "${EXTRA_ARGS[@]}" "$FILTER" \
    | tee "$PROJECT_ROOT/benchmarks/c2-strings-latest.log"

echo ""
echo -e "${GREEN}Done.${NC}"
echo "  log:    benchmarks/c2-strings-latest.log"
echo "  report: target/criterion/report/index.html"
