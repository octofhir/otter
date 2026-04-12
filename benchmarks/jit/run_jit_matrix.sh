#!/usr/bin/env bash
# JIT Benchmark Matrix — compare Otter vs Node vs Bun vs Deno
#
# Usage:
#   bash benchmarks/jit/run_jit_matrix.sh [--benchmarks <filter>] [--runtimes <list>] [--runs <N>]
#
# Output: JSON array to stdout, human-readable table to stderr.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_DIR="$REPO_ROOT/benchmarks/results/jit"

BENCHMARK_FILTER=""
RUNTIMES_CSV="otter,node,bun,deno"
TIMEOUT_SEC=30
MEASURED_RUNS=3

while [[ $# -gt 0 ]]; do
    case $1 in
        --benchmarks) BENCHMARK_FILTER="$2"; shift 2 ;;
        --runtimes)   RUNTIMES_CSV="$2";     shift 2 ;;
        --timeout)    TIMEOUT_SEC="$2";       shift 2 ;;
        --runs)       MEASURED_RUNS="$2";     shift 2 ;;
        *) shift ;;
    esac
done

# ---- Runtime detection ----

get_cmd() {
    local rt="$1"
    case "$rt" in
        otter)
            local bin="$REPO_ROOT/target/release/otterjs"
            if [[ -x "$bin" ]]; then echo "$bin run"; return 0; fi
            bin="$REPO_ROOT/target/debug/otterjs"
            if [[ -x "$bin" ]]; then echo "$bin run"; return 0; fi
            return 1 ;;
        node)
            command -v node >/dev/null 2>&1 && echo "node --experimental-strip-types" && return 0
            return 1 ;;
        bun)
            command -v bun >/dev/null 2>&1 && echo "bun run" && return 0
            return 1 ;;
        deno)
            command -v deno >/dev/null 2>&1 && echo "deno run --allow-all" && return 0
            return 1 ;;
    esac
    return 1
}

get_version() {
    local rt="$1"
    case "$rt" in
        otter) ${cmd%% *} --version 2>/dev/null || echo "dev" ;;
        node)  node --version 2>/dev/null || echo "?" ;;
        bun)   bun --version 2>/dev/null || echo "?" ;;
        deno)  deno --version 2>/dev/null | head -1 || echo "?" ;;
    esac
}

# Detect available runtimes
IFS=',' read -r -a RT_LIST <<< "$RUNTIMES_CSV"
AVAILABLE=()
CMDS=()
for rt in "${RT_LIST[@]}"; do
    cmd=$(get_cmd "$rt" 2>/dev/null) && {
        AVAILABLE+=("$rt")
        CMDS+=("$cmd")
        >&2 echo "[matrix] Found: $rt -> $cmd"
    } || {
        >&2 echo "[matrix] Skipping: $rt (not found)"
    }
done

if [[ ${#AVAILABLE[@]} -eq 0 ]]; then
    >&2 echo "[matrix] ERROR: No runtimes available."
    exit 1
fi

# Gather benchmarks
BENCHMARKS=()
for f in "$SCRIPT_DIR"/*.ts; do
    bname="$(basename "$f" .ts)"
    if [[ -n "$BENCHMARK_FILTER" && "$bname" != *"$BENCHMARK_FILTER"* ]]; then
        continue
    fi
    BENCHMARKS+=("$bname")
done

if [[ ${#BENCHMARKS[@]} -eq 0 ]]; then
    >&2 echo "[matrix] No benchmarks found."
    exit 1
fi

>&2 echo "[matrix] Benchmarks: ${BENCHMARKS[*]}"
>&2 echo "[matrix] Runs: $MEASURED_RUNS measured"
>&2 echo ""

# Table header
>&2 printf "%-24s" "Benchmark"
for rt in "${AVAILABLE[@]}"; do
    >&2 printf "%12s" "$rt"
done
>&2 echo ""
>&2 printf "%-24s" "------------------------"
for _rt in "${AVAILABLE[@]}"; do
    >&2 printf "%12s" "----------"
done
>&2 echo ""

mkdir -p "$RESULTS_DIR"
JSON_RESULTS="["
FIRST=true

# ---- Run matrix ----

for bench in "${BENCHMARKS[@]}"; do
    >&2 printf "%-24s" "$bench"

    for idx in "${!AVAILABLE[@]}"; do
        rt="${AVAILABLE[$idx]}"
        cmd="${CMDS[$idx]}"
        file="$SCRIPT_DIR/${bench}.ts"

        total_ms=0
        ok=true
        for (( r=0; r<MEASURED_RUNS; r++ )); do
            start_ns=$(python3 -c 'import time; print(int(time.time_ns()))' 2>/dev/null || date +%s%N)
            if timeout "$TIMEOUT_SEC" $cmd "$file" >/dev/null 2>&1; then
                end_ns=$(python3 -c 'import time; print(int(time.time_ns()))' 2>/dev/null || date +%s%N)
                elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))
                total_ms=$((total_ms + elapsed_ms))
            else
                ok=false
                break
            fi
        done

        if $ok; then
            avg_ms=$((total_ms / MEASURED_RUNS))
            >&2 printf "%10s ms" "$avg_ms"
            avg_val="$avg_ms"
            status="ok"
        else
            >&2 printf "%12s" "FAIL"
            avg_val="null"
            status="fail"
        fi

        version=$(get_version "$rt" 2>/dev/null || echo "?")
        if [[ "$FIRST" == "true" ]]; then FIRST=false; else JSON_RESULTS+=","; fi
        JSON_RESULTS+="
  {\"runtime\":\"$rt\",\"benchmark\":\"$bench\",\"avg_ms\":$avg_val,\"runs\":$MEASURED_RUNS,\"status\":\"$status\"}"
    done
    >&2 echo ""
done

JSON_RESULTS+="
]"

TIMESTAMP=$(date +%Y%m%d_%H%M%S)
RESULT_FILE="$RESULTS_DIR/jit_matrix_${TIMESTAMP}.json"
echo "$JSON_RESULTS" > "$RESULT_FILE"
>&2 echo ""
>&2 echo "[matrix] Results saved to: $RESULT_FILE"
echo "$JSON_RESULTS"
