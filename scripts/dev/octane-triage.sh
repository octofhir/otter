#!/usr/bin/env bash
#
# Triage Octane suites across tiers: one score per mode (interpreter, jitless,
# tiered) plus a tiered run with --jit-events summarized by jitevents.py.
#
#   scripts/dev/octane-triage.sh [suite...]
#
# Output: benchmarks/results/octane-triage/<suite>.txt (ignored).

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/benchmarks/results/octane-triage"
mkdir -p "$OUT"
if [ "$#" -gt 0 ]; then
  SUITES=("$@")
else
  SUITES=(richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes \
          pdfjs mandreel gbemu code-load box2d zlib typescript)
fi

for suite in "${SUITES[@]}"; do
  report="$OUT/$suite.txt"
  : > "$report"
  for mode in interpreter jitless tiered; do
    "$ROOT/scripts/dev/octane1.sh" "$suite" "$mode" 2>&1 | tail -1 >> "$report"
  done
  events="$OUT/$suite-events.json"
  "$ROOT/scripts/dev/octane1.sh" "$suite" tiered "--jit-events=$events" > /dev/null 2>&1
  [ -f "$events" ] && python3 "$ROOT/scripts/dev/jitevents.py" "$events" 10 >> "$report"
  head -3 "$report"
done
