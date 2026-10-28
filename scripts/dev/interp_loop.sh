#!/usr/bin/env bash
# Retired instructions per interpreted op on a warm loop under the plain
# interpreter (`--interpreter`, no feedback): the loop body's op count comes
# from the bytecode dump, the empty program is subtracted.
# usage: scripts/dev/interp_loop.sh <binary> [iterations]
O=${1:?binary}; N=${2:-5000000}
D=$(mktemp -d)
cat >"$D/loop.js" <<JS
function f(n) { let s = 0, t = 1; for (let k = 0; k < n; k++) { s = (s + k * 3) | 0; t = t ^ s; if (s > 1000) s -= 7; } return s + t; }
console.log(f($N));
JS
cat >"$D/empty.js" <<JS
function f(n) { let s = 0, t = 1; for (let k = 0; k < n; k++) { s = (s + k * 3) | 0; t = t ^ s; if (s > 1000) s -= 7; } return s + t; }
console.log(f(0));
JS
instr() { /usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/instructions retired/ {print $1}'; }
cyc() { /usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/cycles elapsed/ {print $1}'; }
a=$(instr "$O" --interpreter "$D/loop.js"); b=$(instr "$O" --interpreter "$D/empty.js")
ca=$(cyc "$O" --interpreter "$D/loop.js"); cb=$(cyc "$O" --interpreter "$D/empty.js")
echo "per-iteration instr $(( (a - b) / N )) cycles $(( (ca - cb) / N ))"
rm -rf "$D"
