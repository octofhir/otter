#!/usr/bin/env bash
# Retired instructions of whole Web Tooling Benchmark runs (20 fixed samples
# per target, so fixed work) for two otter binaries: a load-independent
# before/after comparison. Needs dist/one.js (scripts/dev/web_tooling_matrix.sh).
# usage: scripts/dev/web_tooling_instr.sh <before-bin> <after-bin> <out-file> [target...]
set -u
abs() { echo "$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"; }
A=$(abs "${1:?before}"); B=$(abs "${2:?after}"); OUT=$(abs "${3:?out}")
shift 3
DIR=benchmarks/.suite-cache/web-tooling-benchmark/dist
T=${*:-acorn babel babel-minify babylon buble chai coffeescript espree esprima jshint lebab postcss prepack prettier source-map terser typescript uglify-js}
instr() { (cd "$DIR" && /usr/bin/time -l "$1" --allow-read one.js "$2" 2>&1 >/dev/null) | awk '/instructions retired/ {print $1}'; }
: >"$OUT"
for t in $T; do
  a=$(instr "$A" "$t"); b=$(instr "$B" "$t")
  python3 -c "print(f'$t {$a/1e9:.2f}G {$b/1e9:.2f}G {100*($b/$a-1):+.1f}%')" | tee -a "$OUT"
done
python3 - "$OUT" <<'EOF'
import math, sys
rows = [l.split() for l in open(sys.argv[1])]
g = math.exp(sum(math.log(float(r[2][:-1]) / float(r[1][:-1])) for r in rows) / len(rows))
print(f"geomean after/before {g:.3f}")
EOF
