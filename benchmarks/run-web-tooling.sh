#!/usr/bin/env bash
#
# Run V8's Web Tooling Benchmark bundle. Lower milliseconds are better.
# This suite must be built by npm in its own checkout.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

WEB_TOOLING_BENCHMARK_DIR="${WEB_TOOLING_BENCHMARK_DIR:-$CACHE_DIR/web-tooling-benchmark}"
ONLY=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --only)
      ONLY="${2:-}"
      [ -n "$ONLY" ] || { echo "error: --only requires a benchmark name" >&2; exit 2; }
      shift 2
      ;;
    --only=*)
      ONLY="${1#--only=}"
      shift
      ;;
    *)
      echo "error: unsupported argument: $1" >&2
      exit 2
      ;;
  esac
done

if [ ! -d "$WEB_TOOLING_BENCHMARK_DIR" ]; then
  echo "cloning Web Tooling Benchmark into $WEB_TOOLING_BENCHMARK_DIR" >&2
  git clone https://github.com/v8/web-tooling-benchmark "$WEB_TOOLING_BENCHMARK_DIR"
fi

if [ ! -d "$WEB_TOOLING_BENCHMARK_DIR/node_modules" ]; then
  echo "installing Web Tooling Benchmark npm dependencies" >&2
  (cd "$WEB_TOOLING_BENCHMARK_DIR" && npm install)
fi

if [ -n "$ONLY" ]; then
  (cd "$WEB_TOOLING_BENCHMARK_DIR" && npx webpack "--env.only=$ONLY")
elif [ ! -f "$WEB_TOOLING_BENCHMARK_DIR/dist/cli.js" ]; then
  (cd "$WEB_TOOLING_BENCHMARK_DIR" && npx webpack)
fi

if [ ! -f "$WEB_TOOLING_BENCHMARK_DIR/dist/cli.js" ]; then
  echo "error: missing Web Tooling Benchmark bundle: dist/cli.js" >&2
  exit 1
fi

OTTER="$(ensure_otter_bin)"
OUT="$RESULTS_DIR/web-tooling-$(timestamp).log"

run_capped() {
  local label="$1"
  shift
  local tmp
  tmp="$(mktemp)"
  echo "=== $label ==="
  set +e
  "$@" > "$tmp" 2>&1
  local status=$?
  set -e
  if [ "$status" -eq 0 ]; then
    cat "$tmp"
  else
    awk 'NR <= 120 { print } END { if (NR > 120) print "... truncated failure output (" NR " lines)" }' "$tmp"
    echo "exit_code: $status"
  fi
  rm -f "$tmp"
}

run_external_cli() {
  local runtime="$1"
  if ! command -v "$runtime" >/dev/null 2>&1; then
    echo "skip: $runtime not found" >&2
    return 0
  fi
  (cd "$WEB_TOOLING_BENCHMARK_DIR" && "$runtime" dist/cli.js)
}

run_otter_cli() {
  local timeout="${OTTER_BENCH_TIMEOUT:-0}"
  (cd "$WEB_TOOLING_BENCHMARK_DIR" && OTTER_JIT="${OTTER_JIT:-1}" "$OTTER" \
    --timeout "$timeout" --allow-read="$WEB_TOOLING_BENCHMARK_DIR" run dist/cli.js)
}

set +e
{
  run_capped node run_external_cli node
  run_capped bun run_external_cli bun
  run_capped otter run_otter_cli
} 2>&1 | tee "$OUT"
pipe_status=("${PIPESTATUS[@]}")
set -e
status="${pipe_status[0]}"

if grep -Eq '^exit_code: [1-9][0-9]*$' "$OUT"; then
  echo "error: Web Tooling Benchmark reported a failure" >&2
  status=1
fi
if ! awk '/^=== otter ===/{in_otter=1; next} /^=== /{in_otter=0} in_otter && /Geometric mean:/{found=1} END{exit found ? 0 : 1}' "$OUT"; then
  echo "error: Otter Web Tooling Benchmark completed without a geometric mean" >&2
  status=1
fi

echo "wrote $OUT" >&2
exit "$status"
