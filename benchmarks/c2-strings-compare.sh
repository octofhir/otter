#!/bin/bash
#
# Otter vs Node.js vs Bun — same C2 string workloads at the JS layer.
#
# Reads benchmarks/cpu/c2-strings.js, runs it under each available runtime
# 3 times, picks the best per (runtime, case), prints a side-by-side
# comparison.
#
# Plain-bash; works on macOS bash 3.2 (no `declare -A`).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
SRC="$SCRIPT_DIR/cpu/c2-strings.js"
ITERS=3

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

OTTER_BIN="$PROJECT_ROOT/target/release/otter"
if [ ! -x "$OTTER_BIN" ]; then
    echo -e "${YELLOW}Building otter (release)...${NC}"
    (cd "$PROJECT_ROOT" && cargo build --release -p otterjs) || {
        echo "otter build failed" >&2; exit 1;
    }
fi

# Collect raw runs into a single TSV: runtime <tab> case <tab> dur_ms.
RAW=$(mktemp)
trap 'rm -f "$RAW"' EXIT

run_runtime() {
    local rt="$1"; shift
    for iter in $(seq 1 $ITERS); do
        # `$@` here is the runtime command (e.g. "node" or "otter run").
        local out
        if ! out="$("$@" "$SRC" 2>/dev/null)"; then
            return 0
        fi
        echo "$out" | awk -F',' -v rt="$rt" '
            /^[a-z_0-9]+,/ { printf "%s\t%s\t%s\n", rt, $1, $2 }
        ' >> "$RAW"
    done
}

echo -e "${BLUE}================================================================${NC}"
echo -e "${BLUE} C2 strings: Otter vs Node vs Bun (${ITERS} iters, best-of)${NC}"
echo -e "${BLUE}================================================================${NC}"

echo -e "${YELLOW}Running on otter ...${NC}"
run_runtime otter "$OTTER_BIN" run

if command -v node >/dev/null 2>&1; then
    echo -e "${YELLOW}Running on node ...${NC}"
    run_runtime node node
fi

if command -v bun >/dev/null 2>&1; then
    echo -e "${YELLOW}Running on bun ...${NC}"
    run_runtime bun bun run
fi

if [ ! -s "$RAW" ]; then
    echo "no benchmark output captured" >&2
    exit 1
fi

# Reduce to best-of per (runtime, case).
BEST=$(mktemp)
trap 'rm -f "$RAW" "$BEST"' EXIT
sort -k1,1 -k2,2 -k3,3g "$RAW" | awk -F'\t' '
    { key = $1 SUBSEP $2 }
    !(key in best) || $3 < best[key] { best[key] = $3 }
    END {
        for (k in best) {
            split(k, parts, SUBSEP)
            printf "%s\t%s\t%s\n", parts[1], parts[2], best[k]
        }
    }
' > "$BEST"

# Print table.
echo ""
printf "${GREEN}%-22s %12s %12s %12s   %s${NC}\n" \
    "case" "otter (ms)" "node (ms)" "bun (ms)" "Otter ÷ best"
printf "%-22s %12s %12s %12s   %s\n" \
    "----" "----------" "---------" "--------" "-----------"

# Stable case order: as they first appeared in RAW.
CASES=$(awk -F'\t' '!seen[$2]++ { print $2 }' "$RAW")

while IFS= read -r case; do
    o=$(awk -F'\t' -v c="$case" '$1=="otter" && $2==c { print $3 }' "$BEST")
    n=$(awk -F'\t' -v c="$case" '$1=="node"  && $2==c { print $3 }' "$BEST")
    b=$(awk -F'\t' -v c="$case" '$1=="bun"   && $2==c { print $3 }' "$BEST")

    o_disp="${o:--}"
    n_disp="${n:--}"
    b_disp="${b:--}"

    ratio="-"
    if [ -n "$o" ]; then
        best=""
        for v in "$n" "$b"; do
            [ -z "$v" ] && continue
            if [ -z "$best" ] || awk "BEGIN { exit !($v < $best) }"; then
                best=$v
            fi
        done
        if [ -n "$best" ] && awk "BEGIN { exit !($best > 0) }"; then
            ratio=$(awk "BEGIN { printf \"%.2fx\", $o / $best }")
        fi
    fi

    printf "%-22s %12s %12s %12s   %s\n" \
        "$case" "$o_disp" "$n_disp" "$b_disp" "$ratio"
done <<< "$CASES"

echo ""
echo -e "${BLUE}Lower is better. 'Otter ÷ best' < 1 means Otter wins; > 1 means slower.${NC}"
