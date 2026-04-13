#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
RESULTS_DIR="$REPO_ROOT/benchmarks/results/jit"
OTTER_BIN="${OTTER_BIN:-$REPO_ROOT/target/release/otter}"
OTTER_TIMEOUT_SECONDS="${OTTER_TIMEOUT_SECONDS:-45}"
TIER1_MAX_MEDIAN_MS="${TIER1_MAX_MEDIAN_MS:-2.0}"
TIER1_MIN_COMPILATIONS="${TIER1_MIN_COMPILATIONS:-1}"
RUNS_PER_BENCH="${RUNS_PER_BENCH:-1}"

BENCHMARKS=(
  "arithmetic_loop"
  "monomorphic_prop"
  "call_chain"
)

benchmark_args() {
  local bench="$1"
  case "$bench" in
    arithmetic_loop) echo "${ARITHMETIC_LOOP_ARGS:-50000 2}" ;;
    monomorphic_prop) echo "${MONOMORPHIC_PROP_ARGS:-20000 2}" ;;
    call_chain) echo "${CALL_CHAIN_ARGS:-25000 2 20 12}" ;;
    *) echo "" ;;
  esac
}

if [[ $# -gt 0 ]]; then
  BENCHMARKS=("$@")
fi

mkdir -p "$RESULTS_DIR"

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  cargo build --release -p otterjs >/dev/null
fi

if [[ ! -x "$OTTER_BIN" ]]; then
  echo "Otter binary not found at $OTTER_BIN" >&2
  exit 2
fi

TIMEOUT_TOOL=""
if [[ "$OTTER_TIMEOUT_SECONDS" != "0" ]]; then
  if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_TOOL="timeout"
  elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_TOOL="gtimeout"
  fi
fi

now_ms() {
  perl -MTime::HiRes=time -e 'printf "%.0f\n", time()*1000'
}

json_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//$'\n'/\\n}"
  printf '%s' "$value"
}

TIMESTAMP="$(date -u +"%Y%m%dT%H%M%SZ")"
RAW_JSON="$RESULTS_DIR/tier1-release-gate-$TIMESTAMP.json"
RAW_TSV="$RESULTS_DIR/tier1-release-gate-$TIMESTAMP.tsv"
DASHBOARD="$RESULTS_DIR/TIER1_RELEASE_GATE_DASHBOARD.md"

otter_ver="$("$OTTER_BIN" --version 2>/dev/null || echo "unavailable")"

echo "benchmark|run|status|exit_code|wall_ms|bench_ms|tier1_compilations|tier1_median_ms|helper_calls_total|log_file|failure_reason" > "$RAW_TSV"

gate_failed=0

for bench in "${BENCHMARKS[@]}"; do
  bench_file="$SCRIPT_DIR/${bench}.ts"
  if [[ ! -f "$bench_file" ]]; then
    echo "Benchmark file not found: $bench_file" >&2
    exit 2
  fi

  for (( run=1; run<=RUNS_PER_BENCH; run++ )); do
    log_file="$RESULTS_DIR/tier1-${TIMESTAMP}-${bench}-run${run}.log"
    raw_args="$(benchmark_args "$bench")"
    bench_args=()
    if [[ -n "$raw_args" ]]; then
      # shellcheck disable=SC2206
      bench_args=($raw_args)
    fi
    start_ms="$(now_ms)"
    set +e
    if [[ -n "$TIMEOUT_TOOL" ]]; then
      "$TIMEOUT_TOOL" "$OTTER_TIMEOUT_SECONDS" \
        "$OTTER_BIN" \
        --timeout "$OTTER_TIMEOUT_SECONDS" \
        --dump-jit-stats \
        run "$bench_file" "${bench_args[@]}" >"$log_file" 2>&1
    else
      "$OTTER_BIN" \
        --timeout "$OTTER_TIMEOUT_SECONDS" \
        --dump-jit-stats \
        run "$bench_file" "${bench_args[@]}" >"$log_file" 2>&1
    fi
    exit_code=$?
    set -e
    end_ms="$(now_ms)"
    wall_ms=$((end_ms - start_ms))

    status="ok"
    failure_reason=""

    if [[ $exit_code -ne 0 ]]; then
      if [[ $exit_code -eq 124 ]] || rg -qi "timed out|timeout|Execution interrupted" "$log_file"; then
        status="timeout"
        failure_reason="runtime-timeout"
      else
        status="error"
        failure_reason="runtime-error"
      fi
    fi

    bench_ms="$(sed -nE "s/^${bench}: ([0-9.]+)ms.*/\\1/p" "$log_file" | tail -n1 || true)"
    tier1_line="$(rg -N "Tier 1: [0-9]+ compilations, median [0-9.]+ms" "$log_file" | tail -n1 || true)"
    tier1_compilations="$(printf '%s\n' "$tier1_line" | sed -nE 's/.*Tier 1: ([0-9]+) compilations, median ([0-9.]+)ms.*/\1/p')"
    tier1_median_ms="$(printf '%s\n' "$tier1_line" | sed -nE 's/.*Tier 1: ([0-9]+) compilations, median ([0-9.]+)ms.*/\2/p')"
    helper_calls_total="$(sed -nE 's/^── Helper Calls \(([0-9]+) total\) ──$/\1/p' "$log_file" | tail -n1 || true)"

    if [[ -z "$bench_ms" ]]; then
      bench_ms=""
      if [[ "$status" == "ok" ]]; then
        status="gate-fail"
        failure_reason="missing-benchmark-output"
      fi
    fi

    if [[ -z "$tier1_compilations" ]]; then
      tier1_compilations=""
      if [[ "$status" == "ok" ]]; then
        status="gate-fail"
        failure_reason="missing-tier1-telemetry"
      fi
    elif (( tier1_compilations < TIER1_MIN_COMPILATIONS )); then
      if [[ "$status" == "ok" ]]; then
        status="gate-fail"
        failure_reason="tier1-not-triggered"
      fi
    fi

    if [[ -z "$tier1_median_ms" ]]; then
      tier1_median_ms=""
      if [[ "$status" == "ok" ]]; then
        status="gate-fail"
        failure_reason="missing-tier1-median"
      fi
    elif ! awk "BEGIN { exit !($tier1_median_ms <= $TIER1_MAX_MEDIAN_MS) }"; then
      if [[ "$status" == "ok" ]]; then
        status="gate-fail"
        failure_reason="tier1-median-too-slow"
      fi
    fi

    if [[ -z "$helper_calls_total" ]]; then
      helper_calls_total="0"
    fi

    if [[ "$status" != "ok" ]]; then
      gate_failed=1
    fi

    echo "${bench}|${run}|${status}|${exit_code}|${wall_ms}|${bench_ms}|${tier1_compilations}|${tier1_median_ms}|${helper_calls_total}|${log_file}|${failure_reason}" >> "$RAW_TSV"
  done
done

{
  echo "{"
  echo "  \"generated_at_utc\": \"$(date -u +"%Y-%m-%dT%H:%M:%SZ")\","
  echo "  \"benchmark_suite\": \"tier1-release-gate\","
  echo "  \"benchmarks\": ["
  for i in "${!BENCHMARKS[@]}"; do
    comma=","
    if [[ "$i" -eq $((${#BENCHMARKS[@]} - 1)) ]]; then
      comma=""
    fi
    printf '    "%s"%s\n' "${BENCHMARKS[$i]}" "$comma"
  done
  echo "  ],"
  echo "  \"runs_per_benchmark\": $RUNS_PER_BENCH,"
  echo "  \"otter_timeout_seconds\": $OTTER_TIMEOUT_SECONDS,"
  echo "  \"tier1_min_compilations\": $TIER1_MIN_COMPILATIONS,"
  echo "  \"tier1_max_median_ms\": $TIER1_MAX_MEDIAN_MS,"
  echo "  \"otter_version\": \"$(json_escape "$otter_ver")\","
  echo "  \"results\": ["
  first=1
  while IFS='|' read -r benchmark run status exit_code wall_ms bench_ms tier1_compilations tier1_median_ms helper_calls_total log_file failure_reason; do
    if [[ "$benchmark" == "benchmark" ]]; then
      continue
    fi
    if [[ $first -eq 0 ]]; then
      echo ","
    fi
    first=0
    bench_ms_json="null"
    tier1_compilations_json="null"
    tier1_median_ms_json="null"
    if [[ -n "$bench_ms" ]]; then
      bench_ms_json="$bench_ms"
    fi
    if [[ -n "$tier1_compilations" ]]; then
      tier1_compilations_json="$tier1_compilations"
    fi
    if [[ -n "$tier1_median_ms" ]]; then
      tier1_median_ms_json="$tier1_median_ms"
    fi
    printf '    {"benchmark":"%s","run":%s,"status":"%s","exit_code":%s,"wall_ms":%s,"bench_ms":%s,"tier1_compilations":%s,"tier1_median_ms":%s,"helper_calls_total":%s,"failure_reason":"%s","log_file":"%s"}' \
      "$(json_escape "$benchmark")" \
      "$run" \
      "$(json_escape "$status")" \
      "$exit_code" \
      "$wall_ms" \
      "$bench_ms_json" \
      "$tier1_compilations_json" \
      "$tier1_median_ms_json" \
      "$helper_calls_total" \
      "$(json_escape "$failure_reason")" \
      "$(json_escape "$log_file")"
  done < "$RAW_TSV"
  echo
  echo "  ]"
  echo "}"
} > "$RAW_JSON"

{
  echo "# Tier 1 Release Gate Dashboard"
  echo
  echo "- Generated (UTC): $(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  echo "- Benchmarks: \`${BENCHMARKS[*]}\`"
  echo "- Runs per benchmark: \`$RUNS_PER_BENCH\`"
  echo "- Otter binary: \`$OTTER_BIN\`"
  echo "- Otter version: \`$otter_ver\`"
  echo "- Tier 1 min compilations: \`$TIER1_MIN_COMPILATIONS\`"
  echo "- Tier 1 median ceiling: \`${TIER1_MAX_MEDIAN_MS}ms\`"
  echo
  echo "| Benchmark | Run | Status | Bench ms | Wall ms | Tier 1 compilations | Tier 1 median ms | Helper calls | Failure |"
  echo "|---|---:|---|---:|---:|---:|---:|---:|---|"
  while IFS='|' read -r benchmark run status _exit_code wall_ms bench_ms tier1_compilations tier1_median_ms helper_calls_total _log_file failure_reason; do
    if [[ "$benchmark" == "benchmark" ]]; then
      continue
    fi
    [[ -n "$bench_ms" ]] || bench_ms="n/a"
    [[ -n "$tier1_compilations" ]] || tier1_compilations="n/a"
    [[ -n "$tier1_median_ms" ]] || tier1_median_ms="n/a"
    [[ -n "$failure_reason" ]] || failure_reason="ok"
    echo "| $benchmark | $run | $status | $bench_ms | $wall_ms | $tier1_compilations | $tier1_median_ms | $helper_calls_total | $failure_reason |"
  done < "$RAW_TSV"
  echo
  echo "Raw data:"
  echo "- JSON: \`$RAW_JSON\`"
  echo "- TSV: \`$RAW_TSV\`"
} > "$DASHBOARD"

echo "Wrote:"
echo "  $RAW_JSON"
echo "  $RAW_TSV"
echo "  $DASHBOARD"

if [[ $gate_failed -ne 0 ]]; then
  echo "FAIL: Tier 1 release gate failed" >&2
  exit 1
fi

echo "PASS: Tier 1 release gate passed"
