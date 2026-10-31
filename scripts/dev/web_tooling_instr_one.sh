#!/usr/bin/env bash
# Retired instructions of whole Web Tooling Benchmark runs for one binary
# (otter, or node when the binary name is `node`), one `target count` line
# per target. Pair two outputs with scripts/dev/instr_ratio.py.
# usage: scripts/dev/web_tooling_instr_one.sh <bin> <out-file> [target...]
set -u
B=${1:?bin}; [ "$B" = node ] || B=$(cd "$(dirname "$B")" && pwd)/$(basename "$B")
OUT=$(cd "$(dirname "${2:?out}")" && pwd)/$(basename "$2")
shift 2
DIR=benchmarks/.suite-cache/web-tooling-benchmark/dist
T=${*:-acorn babel babel-minify babylon buble chai coffeescript espree esprima jshint lebab postcss prepack prettier source-map terser typescript uglify-js}
flags=--allow-read; [ "$B" = node ] && flags=
: >"$OUT"
for t in $T; do
  n=$( (cd "$DIR" && /usr/bin/time -l "$B" $flags one.js "$t" 2>&1 >/dev/null) | awk '/instructions retired/ {print $1}')
  echo "$t $n" | tee -a "$OUT"
done
