#!/usr/bin/env bash
# Retired instructions per megamorphic property operation over 12 shapes:
# each loop runs R rounds over 1200 objects; the empty loop is subtracted.
# usage: scripts/dev/megamorphic_instr.sh <binary> [R]
O=${1:?binary}; R=${2:-5000}
D=$(mktemp -d)
prelude() {
  cat <<JS
const makers = [];
for (let k = 0; k < 12; k++) {
  const fields = Array.from({ length: k + 1 }, (_, j) => 'f' + j);
  makers.push(new Function('x', 'this.kind = x; ' + fields.map((f) => 'this.' + f + ' = 0;').join(' ') + ' this.parent = null;'));
}
const objs = [];
for (let i = 0; i < 1200; i++) objs.push(new makers[i % 12](i));
let s = 0;
JS
}
body() {
  prelude
  echo "function f(v, k) { for (let i = 0; i < objs.length; i++) { $1 } }"
  echo "for (let r = 0; r < $R; r++) f(r, 'kind');"
  echo "console.log(s);"
}
body 's += i;' >"$D/empty.js"
body 's += i; objs[i].parent = v;' >"$D/store.js"
body 's += objs[i].kind;' >"$D/load.js"
body 's += objs[i][k];' >"$D/keyed.js"
body 's += i; objs[i][k] = v;' >"$D/keyedStore.js"
instr() { /usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/instructions retired/ {print $1}'; }
base=$(instr "$O" "$D/empty.js")
for f in store load keyed keyedStore; do
  n=$(instr "$O" "$D/$f.js")
  echo "$f $(( (n - base) / (R * 1200) )) instr/op"
done
rm -rf "$D"
