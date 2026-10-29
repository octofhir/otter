#!/usr/bin/env bash
# Retired instructions per non-inlined call: N calls per shape of call, the
# same loop without the call subtracted.
# usage: scripts/dev/call_instr.sh <binary> [N]
O=${1:?binary}; N=${2:-20000000}
D=benchmarks/results/call-instr; mkdir -p "$D"
prelude() {
  cat <<JS
function big(x) { if (x === -1) { let t = 0; $(for k in $(seq 1 60); do printf 'for (let i = 0; i < 100; i++) { t += i * $k; t ^= i; } '; done) return t; } return x + 1; }
function C(x) { this.x = x; this.y = x; if (x === -1) { for (let i = 0; i < 100; i++) this.y += i * i; } }
const protoA = { m(x) { if (x === -1) { let t = 0; for (let i = 0; i < 100; i++) { t += i * i; t ^= i; } return t; } return x + 1; } };
const objs = [];
const keep = new Array(1024).fill(null);
for (let k = 0; k < 4; k++) { const o = Object.create(protoA); o['p' + k] = k; objs.push(o); }
let out = 0;
JS
}
body() {
  prelude
  echo "function f(n) { let s = 0; for (let i = 0; i < n; i++) { $1 } out += s; }"
  echo "f(1000); f($N);"
  echo "console.log(out);"
}
body 's = (s + 1) | 0;' >"$D/empty.js"
body 's = (s + 1) | 0; keep[i & 1023] = objs[i & 3];' >"$D/storeonly.js"
body 's = big(s) | 0;' >"$D/direct.js"
body 's = objs[i & 3].m(s) | 0;' >"$D/method.js"
body 's = (s + 1) | 0; keep[i & 1023] = new C(i);' >"$D/construct.js"
instr() { /usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/instructions retired/ {print $1}'; }
for e in "$O" node bun; do
  base=$(instr $e "$D/empty.js")
  line="$(basename $e)"
  for t in direct method construct storeonly; do
    v=$(instr $e "$D/$t.js")
    line="$line $t=$(( (v - base) / N ))"
  done
  echo "$line"
done
