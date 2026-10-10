#!/usr/bin/env bash
# Fixed-work instructions of one otter binary against node's recorded counts.
# usage: scripts/dev/fw_vs_node.sh <otter-binary> [fixture...]
# Node's counts come from benchmarks/fixtures/fixed-work/node-instructions.txt
# ("<fixture> <instructions>" per line, retired instructions of `node <fixture>`
# measured with `/usr/bin/time -l` on the reference machine); otter runs every
# fixture once and prints otter/node per fixture and the geometric mean. A
# failing fixture prints its exit code instead.
set -u
O=${1:?binary}; shift
NODE=benchmarks/fixtures/fixed-work/node-instructions.txt
R=benchmarks/results/fw-vs-node
mkdir -p "$R"
FIX=${*:-$(awk '{print $1}' "$NODE")}
for f in $FIX; do
  /usr/bin/time -l "$O" benchmarks/fixtures/fixed-work/$f.js >"$R/o_$f" 2>"$R/t_$f"
  code=$?
  otter=$(grep 'instructions retired' "$R/t_$f" | awk '{print $1}')
  node=$(awk -v f="$f" '$1 == f {print $2}' "$NODE")
  echo "$f $code ${otter:-0} ${node:-0}"
done | awk '
  { if ($2 != 0 || $3 == 0 || $4 == 0) { printf "%-14s exit=%s\n", $1, $2; next }
    r = $3 / $4; s += log(r); n++
    printf "%-14s otter=%6.2fG node=%6.2fG otter/node=%5.2fx\n", $1, $3 / 1e9, $4 / 1e9, r }
  END { if (n) printf "%-14s %31s %5.2fx\n", "geomean", "", exp(s / n) }'
