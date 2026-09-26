#!/usr/bin/env bash
#
# Median Octane score per suite for two binaries (before/after), tiered mode.
#
#   scripts/dev/octane-ab.sh <before-bin> <after-bin> [suite...]
#
# RUNS (default 3) runs per binary; MODE (default tiered).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
before="$1"; after="$2"; shift 2
if [ "$#" -gt 0 ]; then SUITES=("$@"); else
  SUITES=(richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes \
          pdfjs mandreel gbemu code-load box2d zlib typescript)
fi
mode="${MODE:-tiered}"
printf '%-14s %10s %10s %8s\n' suite before after ratio
for s in "${SUITES[@]}"; do
  b=$(OTTER_BIN="$before" RUNS="${RUNS:-3}" "$ROOT/scripts/dev/octane1.sh" "$s" "$mode" 2>/dev/null | sed -n 's/.*median: //p')
  a=$(OTTER_BIN="$after" RUNS="${RUNS:-3}" "$ROOT/scripts/dev/octane1.sh" "$s" "$mode" 2>/dev/null | sed -n 's/.*median: //p')
  r=$(awk -v a="${a:-0}" -v b="${b:-0}" 'BEGIN { if (b > 0) printf "%.2fx", a / b; else print "-" }')
  printf '%-14s %10s %10s %8s\n' "$s" "${b:--}" "${a:--}" "$r"
done
