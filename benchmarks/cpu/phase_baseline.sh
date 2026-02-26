#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
BENCH_FILE="$REPO_ROOT/benchmarks/cpu/flamegraph.ts"
RESULTS_DIR="$REPO_ROOT/benchmarks/results"
OTTER_BIN="${OTTER_BIN:-$REPO_ROOT/target/release/otter}"
OTTER_TIMEOUT_SECONDS="${OTTER_TIMEOUT_SECONDS:-45}"
SLOW_PHASE_THRESHOLD_MS="${SLOW_PHASE_THRESHOLD_MS:-25000}"
mkdir -p "$RESULTS_DIR"

if ! [[ "$OTTER_TIMEOUT_SECONDS" =~ ^[0-9]+$ ]]; then
  echo "OTTER_TIMEOUT_SECONDS must be an integer (got: $OTTER_TIMEOUT_SECONDS)" >&2
  exit 2
fi
if (( OTTER_TIMEOUT_SECONDS > 45 )); then
  echo "OTTER_TIMEOUT_SECONDS must be <= 45 for regression comparability (got: $OTTER_TIMEOUT_SECONDS)" >&2
  exit 2
fi

now_ms() {
  perl -MTime::HiRes=time -e 'printf "%.0f\n", time()*1000'
}

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  cargo build --release -p otterjs >/dev/null
fi

TIMESTAMP="$(date -u +"%Y%m%dT%H%M%SZ")"
RAW_JSON="$RESULTS_DIR/phase-baseline-$TIMESTAMP.json"
RAW_TSV="$RESULTS_DIR/phase-baseline-$TIMESTAMP.tsv"
DASHBOARD="$RESULTS_DIR/PHASE_REGRESSION_DASHBOARD.md"

otter_ver="$("$OTTER_BIN" --version 2>/dev/null || echo "unavailable")"
node_ver="$(node -v 2>/dev/null || echo "unavailable")"
bun_ver="$(bun -v 2>/dev/null || echo "unavailable")"
deno_ver="$(deno --version 2>/dev/null | head -n1 || echo "unavailable")"

runtimes=(otter node bun deno)
phases=(math objects arrays strings calls json)

echo "runtime|phase|status|exit_code|wall_ms|phase_ms|perf_flag|log_file" > "$RAW_TSV"

for runtime in "${runtimes[@]}"; do
  for phase in "${phases[@]}"; do
    log_file="$RESULTS_DIR/phase-${TIMESTAMP}-${runtime}-${phase}.log"
    start_ms="$(now_ms)"
    set +e
    case "$runtime" in
      otter)
        "$OTTER_BIN" --timeout "$OTTER_TIMEOUT_SECONDS" run "$BENCH_FILE" "$phase" 1 >"$log_file" 2>&1
        ;;
      node)
        node --experimental-strip-types "$BENCH_FILE" "$phase" 1 >"$log_file" 2>&1
        ;;
      bun)
        bun "$BENCH_FILE" "$phase" 1 >"$log_file" 2>&1
        ;;
      deno)
        deno run "$BENCH_FILE" "$phase" 1 >"$log_file" 2>&1
        ;;
      *)
        echo "unknown runtime: $runtime" >&2
        exit 1
        ;;
    esac
    exit_code=$?
    set -e
    end_ms="$(now_ms)"
    wall_ms=$((end_ms - start_ms))

    status="ok"
    if [[ $exit_code -ne 0 ]]; then
      if rg -qi "timed out|timeout" "$log_file"; then
        status="timeout"
      else
        status="error"
      fi
    fi

    phase_ms="$(rg -n "^${phase}: " "$log_file" | tail -n1 | sed -E 's/^.*: ([0-9.]+)ms.*/\1/' || true)"
    if [[ -z "${phase_ms:-}" ]]; then
      phase_ms=""
    fi

    perf_flag="n/a"
    if [[ "$runtime" == "otter" ]]; then
      if [[ "$status" == "timeout" ]]; then
        perf_flag="critical-timeout"
      elif [[ "$status" == "ok" && -n "$phase_ms" ]] && awk "BEGIN {exit !($phase_ms > $SLOW_PHASE_THRESHOLD_MS)}"; then
        perf_flag="bad-slow"
      else
        perf_flag="ok"
      fi
    fi

    echo "${runtime}|${phase}|${status}|${exit_code}|${wall_ms}|${phase_ms}|${perf_flag}|${log_file}" >> "$RAW_TSV"
  done
done

{
  echo "{"
  echo "  \"generated_at_utc\": \"$(date -u +"%Y-%m-%dT%H:%M:%SZ")\","
  echo "  \"benchmark\": \"benchmarks/cpu/flamegraph.ts\","
  echo "  \"scale\": 1,"
  echo "  \"otter_timeout_seconds\": $OTTER_TIMEOUT_SECONDS,"
  echo "  \"versions\": {"
  echo "    \"otter\": \"$otter_ver\","
  echo "    \"node\": \"$node_ver\","
  echo "    \"bun\": \"$bun_ver\","
  echo "    \"deno\": \"$deno_ver\""
  echo "  },"
  echo "  \"results\": ["
  first=1
  while IFS='|' read -r runtime phase status exit_code wall_ms phase_ms perf_flag log_file; do
    if [[ "$runtime" == "runtime" ]]; then
      continue
    fi
    if [[ $first -eq 0 ]]; then
      echo ","
    fi
    first=0
    phase_json="null"
    if [[ -n "$phase_ms" ]]; then
      phase_json="$phase_ms"
    fi
    printf '    {"runtime":"%s","phase":"%s","status":"%s","exit_code":%s,"wall_ms":%s,"phase_ms":%s,"perf_flag":"%s","log_file":"%s"}' \
      "$runtime" "$phase" "$status" "$exit_code" "$wall_ms" "$phase_json" "$perf_flag" "$log_file"
  done < "$RAW_TSV"
  echo
  echo "  ]"
  echo "}"
} > "$RAW_JSON"

{
  echo "# Phase Regression Dashboard"
  echo
  echo "- Generated (UTC): $(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  echo "- Benchmark: \`benchmarks/cpu/flamegraph.ts\` (phase mode, scale=1)"
  echo "- Otter timeout: ${OTTER_TIMEOUT_SECONDS}s (\`--timeout ${OTTER_TIMEOUT_SECONDS}\`)"
  echo "- Otter binary: \`$OTTER_BIN\`"
  echo "- Versions: otter=\`$otter_ver\`, node=\`$node_ver\`, bun=\`$bun_ver\`, deno=\`$deno_ver\`"
  echo
  echo "## Results"
  echo
  echo "| Runtime | Phase | Status | Perf flag | Phase ms | Wall ms |"
  echo "|---|---|---|---|---:|---:|"
  while IFS='|' read -r runtime phase status _exit_code wall_ms phase_ms perf_flag _log_file; do
    if [[ "$runtime" == "runtime" ]]; then
      continue
    fi
    if [[ -z "$phase_ms" ]]; then
      phase_ms="n/a"
    fi
    echo "| $runtime | $phase | $status | $perf_flag | $phase_ms | $wall_ms |"
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
