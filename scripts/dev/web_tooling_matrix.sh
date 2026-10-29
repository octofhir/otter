#!/usr/bin/env bash
# Runs/s of otter and node on each Web Tooling Benchmark target (real parsers,
# compilers, minifiers and formatters; 20 fixed samples each), one fresh
# process per target. Bun rejects the bundle (`const Terser` reassignment).
# usage: scripts/dev/web_tooling_matrix.sh <otter-binary> <out-file> [target...]
# Needs the full bundle: benchmarks/.suite-cache/web-tooling-benchmark/dist/cli.js
set -u
O=$(cd "$(dirname "${1:?otter binary}")" && pwd)/$(basename "$1")
OUT=$(cd "$(dirname "${2:?out file}")" && pwd)/$(basename "$2")
shift 2
DIR=benchmarks/.suite-cache/web-tooling-benchmark/dist
T=${*:-acorn babel babel-minify babylon buble chai coffeescript espree esprima jshint lebab postcss prepack prettier source-map terser typescript uglify-js}
# The bundle's `process` is a shim: a copy selects its one target through a
# global instead of argv.
python3 - "$DIR" <<'EOF'
import os, sys
d = sys.argv[1]
s = open(f"{d}/cli.js").read()
old = '''    } else if (false) {
      return [ONLY];'''
if old in s:
    s = s.replace(old, '''    } else if (typeof globalThis.WTB_ONLY == "string") {
      return [globalThis.WTB_ONLY];''')
open(f"{d}/cli-one.js", "w").write(s)
open(f"{d}/one.js", "w").write('globalThis.WTB_ONLY = process.argv[2];\nrequire("./cli-one.js");\n')
EOF
: >"$OUT"
for t in $T; do
  for e in otter node; do
    if [ $e = otter ]; then cmd=("$O" --allow-read); else cmd=(node); fi
    r=$(cd "$DIR" && timeout 600 "${cmd[@]}" one.js "$t" 2>&1 | awk -v t="$t" '$1 == t":" {print $2}')
    echo "$t $e ${r:-FAIL}" | tee -a "$OUT"
  done
done
python3 - "$OUT" <<'EOF'
import math, sys
rows = {}
for line in open(sys.argv[1]):
    t, e, r = line.split()
    rows.setdefault(t, {})[e] = r
print(f"{'target':14} {'otter':>8} {'node':>8} {'node/otter':>10}")
logs = []
for t, r in rows.items():
    try:
        o, n = float(r["otter"]), float(r["node"])
        logs.append(math.log(n / o)); ratio = f"{n / o:.2f}"
    except (KeyError, ValueError):
        ratio = "-"
    print(f"{t:14} {r.get('otter', '-'):>8} {r.get('node', '-'):>8} {ratio:>10}")
if logs:
    print(f"geomean node/otter {math.exp(sum(logs) / len(logs)):.2f} over {len(logs)}")
EOF
