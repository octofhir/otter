#!/usr/bin/env bash
# Fixed-work matrix: exit, stdout head, instructions retired, wall time.
# usage: scripts/dev/fw.sh <otter-binary> [fixture...]
set -u
O=${1:?binary}; shift
R=benchmarks/results/claude-s1/fw
mkdir -p "$R"
FIX=${*:-fib mega_method ast_ctor crypto earley-boyer zlib ts}
for f in $FIX; do
  /usr/bin/time -l "$O" benchmarks/fixtures/fixed-work/$f.js >"$R/o_$f" 2>"$R/t_$f"
  code=$?
  printf "%-13s exit=%s out=%-30s instr=%s real=%s\n" "$f" "$code" "$(head -c 28 "$R/o_$f" | tr '\n' ' ')" \
    "$(grep 'instructions retired' "$R/t_$f" | awk '{print $1}')" "$(grep ' real' "$R/t_$f" | awk '{print $1}')"
done
