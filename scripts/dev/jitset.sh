#!/usr/bin/env bash
# usage: jitset.sh <arch aarch64|x86> <gc levels...>
cd "$(dirname "$0")/../.."
S=scripts/dev
T=""; for t in $(cat $S/jit_set.txt); do T="$T --test $t"; done
arch=$1; shift; tgt=""; [ "$arch" = x86 ] && tgt="--target x86_64-apple-darwin"
for g in "$@"; do mkdir -p benchmarks/results/jitset; out=benchmarks/results/jitset/$arch-gc-$g.log
  if [ $g = unset ]; then env -u OTTER_GC_STRESS cargo test -q --no-fail-fast -p otter-runtime $tgt $T > $out 2>&1; else OTTER_GC_STRESS=$g cargo test -q --no-fail-fast -p otter-runtime $tgt $T > $out 2>&1; fi
  echo "$arch gc=$g $(grep -E 'test result' $out | awk '{p+=$4; f+=$6} END {print "passed="p" failed="f}')"
  grep -E "^\S+ --- FAILED|panicked at" $out | head -20
done
