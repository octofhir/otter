#!/usr/bin/env bash
# Main-thread axis shares of one otter binary on the fixed-work fixtures.
# usage: scripts/dev/axis_profile.sh <otter-binary> <out-dir> [fixture...]
# Samples each fixture's whole run at 1 ms with macOS `sample` into
# <out-dir>/sample-<fixture>.txt and prints scripts/dev/axis_shares.py over them.
set -u
O=${1:?otter binary}; OUT=${2:?out dir}; shift 2
FIX=${*:-richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes box2d gbemu mandreel zlib ts}
mkdir -p "$OUT"
for f in $FIX; do
  "$O" benchmarks/fixtures/fixed-work/$f.js >/dev/null &
  pid=$!
  sample $pid 30 1 -file "$OUT/sample-$f.txt" >/dev/null 2>&1
  wait $pid
done
python3 scripts/dev/axis_shares.py $(for f in $FIX; do echo "$OUT/sample-$f.txt"; done)
