#!/usr/bin/env bash
# Wall-clock, peak RSS and retired instructions of otter, node and bun on the
# fixed-work fixtures: fresh processes, engine order rotated per fixture.
# usage: scripts/dev/wallclock_matrix.sh <otter-binary> <out-dir> [runs] [fixture...]
# Writes <out-dir>/<fixture>.json (hyperfine, raw samples), <out-dir>/rss.txt
# ("<fixture> <engine> <max-rss-bytes> <instructions>" from one /usr/bin/time -l
# run each) and prints a median table with otter/node and otter/bun ratios.
set -u
O=${1:?otter binary}; OUT=${2:?out dir}; RUNS=${3:-5}; shift 3 2>/dev/null || shift $#
FIX=${*:-richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes box2d gbemu mandreel zlib ts}
mkdir -p "$OUT"
: >"$OUT/rss.txt"
i=0
for f in $FIX; do
  js=benchmarks/fixtures/fixed-work/$f.js
  # An engine that fails the fixture is left out of its timing, not timed.
  engines=()
  for e in "$O" node bun; do
    if "$e" "$js" >/dev/null 2>&1; then engines+=("$e $js"); else echo "$f $(basename "$e") FAILED" >&2; fi
  done
  # Rotate which engine runs first.
  n=${#engines[@]}
  rot=("${engines[@]:$((i % n))}" "${engines[@]:0:$((i % n))}")
  hyperfine -N --warmup 1 --runs "$RUNS" --export-json "$OUT/$f.json" "${rot[@]}" >/dev/null 2>&1
  for e in "$O" node bun; do
    /usr/bin/time -l $e "$js" >/dev/null 2>"$OUT/time-$f-$(basename "$e")"
    echo "$f $(basename "$e") $(awk '/maximum resident/ {print $1}' "$OUT/time-$f-$(basename "$e")") $(awk '/instructions retired/ {print $1}' "$OUT/time-$f-$(basename "$e")")" >>"$OUT/rss.txt"
  done
  i=$((i + 1))
done
python3 - "$OUT" "$O" $FIX <<'EOF'
import json, math, sys
out, otter, fixtures = sys.argv[1], sys.argv[2], sys.argv[3:]
rss = {}
for line in open(f"{out}/rss.txt"):
    f, e, m, n = (line.split() + ["0", "0"])[:4]
    rss[(f, e)] = (int(m or 0), int(n or 0))
print(f"{'fixture':14} {'otter ms':>9} {'node ms':>8} {'bun ms':>8} {'o/node':>7} {'o/bun':>6} {'oRSS MB':>8} {'nRSS':>6} {'bRSS':>6}")
logs = {"node": [], "bun": []}
for f in fixtures:
    data = json.load(open(f"{out}/{f}.json"))["results"]
    med = {}
    for r in data:
        engine = r["command"].split()[0]
        key = "otter" if engine == otter else engine
        med[key] = r["median"] * 1000
    o, n, b = med.get("otter"), med.get("node"), med.get("bun")
    if n: logs["node"].append(math.log(o / n))
    if b: logs["bun"].append(math.log(o / b))
    mb = lambda e: rss.get((f, e), (0, 0))[0] / 2**20
    ms = lambda v: f"{v:.0f}" if v else "fail"
    ratio = lambda v: f"{o / v:.2f}" if v else "-"
    print(f"{f:14} {o:9.0f} {ms(n):>8} {ms(b):>8} {ratio(n):>7} {ratio(b):>6} {mb(otter.split('/')[-1]):8.0f} {mb('node'):6.0f} {mb('bun'):6.0f}")
g = lambda k: math.exp(sum(logs[k]) / len(logs[k]))
print(f"{'geomean':14} {'':9} {'':8} {'':8} {g('node'):7.2f} {g('bun'):6.2f}  (bun over {len(logs['bun'])} fixtures)")
EOF
