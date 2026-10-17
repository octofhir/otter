#!/bin/bash
# Retired instructions of two otter binaries on the fixed-work fixtures,
# interleaved: one /usr/bin/time -l run each, `runs` rounds.
# usage: scripts/dev/instr_ab.sh <otter-a> <otter-b> [runs] [fixture...]
A=$1; B=$2; RUNS=${3:-2}; shift 3 2>/dev/null || shift $#
FIX=${*:-richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes box2d gbemu mandreel zlib ts}
instr() { /usr/bin/time -l "$1" "$2" 2>&1 >/dev/null | awk '/instructions retired/ {print $1}'; }
printf "%-14s %14s %14s %8s\n" fixture a b b/a
for f in $FIX; do
  js=benchmarks/fixtures/fixed-work/$f.js
  a=999999999999999; b=999999999999999
  for r in $(seq "$RUNS"); do
    x=$(instr "$A" "$js"); y=$(instr "$B" "$js")
    [ -n "$x" ] && [ "$x" -lt "$a" ] && a=$x
    [ -n "$y" ] && [ "$y" -lt "$b" ] && b=$y
  done
  printf "%-14s %14d %14d %8.3f\n" "$f" "$a" "$b" "$(echo "$b / $a" | bc -l)"
done
