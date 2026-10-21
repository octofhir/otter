#!/usr/bin/env zsh
# A/B of two otter binaries on the fixed-work fixtures: the median of RUNS
# runs (default 5) of retired instructions and of wall-clock milliseconds.
# The two binaries alternate run by run, so drift in machine load reaches
# both alike. Retired instructions vary about 3% between runs (background
# compiles finish at different points), so compare medians, never one run.
# usage: [RUNS=n] scripts/dev/ab_measure.sh <base-binary> <new-binary> [fixture...]
zmodload zsh/datetime
base=$1 new=$2
shift 2
fixtures=($@)
(( $#fixtures )) || fixtures=(richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes box2d gbemu mandreel zlib ts)
runs=${RUNS:-5}
middle=$(( (runs + 1) / 2 ))
for f in $fixtures; do
  js=benchmarks/fixtures/fixed-work/$f.js
  typeset -A instrs walls
  instrs=() walls=()
  for (( i = 0; i < runs; i++ )); do
    for which in base new; do
      bin=${(P)which}
      s=$EPOCHREALTIME
      instr=$( { /usr/bin/time -l $bin $js > /dev/null; } 2>&1 | awk '/instructions retired/ {print $1}')
      e=$EPOCHREALTIME
      wall=$(( (e - s) * 1000 ))
      instrs[$which]+="$instr "
      walls[$which]+="${wall%%.*} "
    done
  done
  for which in base new; do
    instr=$(printf '%s\n' ${=instrs[$which]} | sort -n | sed -n "${middle}p")
    wall=$(printf '%s\n' ${=walls[$which]} | sort -n | sed -n "${middle}p")
    printf '%-14s %-4s instr %6.2fG  wall %5d ms  (%s)\n' $f $which $(( instr / 1e9 )) $wall "${walls[$which]% }"
  done
done
