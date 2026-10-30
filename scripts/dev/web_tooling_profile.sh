#!/usr/bin/env bash
# `sample` profile of otter on each Web Tooling Benchmark target: one fresh
# process per target, sampled for a few seconds after start-up, one report per
# target in <out-dir>. Aggregate with scripts/dev/web_tooling_hot.py.
# usage: scripts/dev/web_tooling_profile.sh <otter-binary> <out-dir> [target...]
# Needs dist/one.js, written by scripts/dev/web_tooling_matrix.sh.
set -u
O=$(cd "$(dirname "${1:?otter binary}")" && pwd)/$(basename "$1")
mkdir -p "${2:?out dir}"
OUT=$(cd "$2" && pwd)
shift 2
DIR=benchmarks/.suite-cache/web-tooling-benchmark/dist
T=${*:-acorn babel babel-minify babylon buble chai coffeescript espree esprima jshint lebab postcss prepack prettier source-map terser typescript uglify-js}
for t in $T; do
  (cd "$DIR" && exec "$O" --allow-read one.js "$t" >/dev/null 2>&1) &
  pid=$!
  sleep 1.5
  sample "$pid" 3 -file "$OUT/$t.txt" >/dev/null 2>&1
  kill "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  echo "$t sampled"
done
