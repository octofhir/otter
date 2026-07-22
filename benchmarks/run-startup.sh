#!/usr/bin/env bash
#
# Track process startup cost on a hello-world: wall time and peak RSS.
#
# Serverless / edge deployment cares about both numbers, and neither shows up
# in a throughput suite — a change that doubles resident memory scores exactly
# the same on richards. Lower is better for both columns.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

REPS="${OTTER_STARTUP_REPS:-10}"
SCRIPT="$CACHE_DIR/startup-hello.js"
printf 'console.log("hello");\n' > "$SCRIPT"

OTTER="$(ensure_otter_bin)"
OUT="$RESULTS_DIR/startup-$(timestamp).log"

# Best-of-N on both columns: the minimum is the least noisy estimator here,
# since scheduler and page-cache noise only ever add time and resident pages.
report_startup() {
  local label="$1"
  shift
  if [ "$#" -eq 0 ]; then
    return 0
  fi
  if ! command -v "$1" >/dev/null 2>&1 && [ ! -x "$1" ]; then
    echo "$label: skipped (not found)"
    return 0
  fi
  local best_ms="" best_rss_kb=""
  for _ in $(seq "$REPS"); do
    local sample
    sample="$(measure_wall_and_rss "$@")" || return 0
    local ms="${sample%% *}"
    local rss_kb="${sample##* }"
    if [ -z "$best_ms" ] || [ "$ms" -lt "$best_ms" ]; then best_ms="$ms"; fi
    if [ -z "$best_rss_kb" ] || [ "$rss_kb" -lt "$best_rss_kb" ]; then best_rss_kb="$rss_kb"; fi
  done
  printf '%-8s wall %6s ms   peak RSS %6s MB\n' \
    "$label" "$best_ms" "$((best_rss_kb / 1024))"
}

{
  echo "=== hello-world startup (best of $REPS) ==="
  report_startup otter "$OTTER" run "$SCRIPT"
  report_startup node node "$SCRIPT"
  report_startup bun bun "$SCRIPT"
} 2>&1 | tee "$OUT"

echo "wrote $OUT" >&2
