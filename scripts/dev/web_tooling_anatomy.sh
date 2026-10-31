#!/usr/bin/env bash
# Generated-code anatomy of Web Tooling Benchmark targets: one fresh process
# per target with `--perf-basic-prof` and `--jit-artifacts`, sampled on its
# isolate thread from start-up until it exits, then scripts/dev/jit_anatomy.py (tier
# residency, instruction buckets, top functions) into <out-dir>/<t>.txt.
# usage: scripts/dev/web_tooling_anatomy.sh <otter-binary> <out-dir> [delay] [secs] [target...]
# Needs dist/one.js, written by scripts/dev/web_tooling_matrix.sh.
set -u
O=$(cd "$(dirname "${1:?otter binary}")" && pwd)/$(basename "$1")
mkdir -p "${2:?out dir}"
OUT=$(cd "$2" && pwd)
DELAY=${3:-1}
SECS=${4:-60}
shift 4 2>/dev/null || shift $#
HERE=$(cd "$(dirname "$0")" && pwd)
DIR=benchmarks/.suite-cache/web-tooling-benchmark/dist
T=${*:-coffeescript lebab chai acorn babylon}
for t in $T; do
  rm -rf "$OUT/$t"
  mkdir -p "$OUT/$t"
  (cd "$DIR" && exec "$O" --allow-read --perf-basic-prof --jit-artifacts="$OUT/$t/art" one.js "$t" >/dev/null 2>&1) &
  pid=$!
  sleep "$DELAY"
  sample "$pid" "$SECS" 1 -file "$OUT/$t/sample.txt" >/dev/null 2>&1
  wait "$pid" 2>/dev/null
  cp "/tmp/perf-$pid.map" "$OUT/$t/perf.map"
  rm -f "/tmp/perf-$pid.map"
  python3 "$HERE/jit_anatomy.py" "$OUT/$t/sample.txt" "$OUT/$t/perf.map" "$OUT/$t/art" 15 > "$OUT/$t.txt"
  echo "$t done"
done
