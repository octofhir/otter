#!/usr/bin/env bash
# Retired instructions per RegExp operation: each loop runs N times and
# the empty loop's count is subtracted.
# usage: scripts/dev/regexp_instr.sh <binary> [N]
O=${1:?binary}; N=${2:-20000000}
D=$(mktemp -d)
cat >"$D/empty.js" <<JS
const ba = /^ba/; let s = 0;
for (let i = 0; i < $N; i++) s += i & 1;
console.log(s);
JS
cat >"$D/test.js" <<JS
const ba = /^ba/; let s = 0;
for (let i = 0; i < $N; i++) if (ba.test('bacde')) s++;
console.log(s);
JS
cat >"$D/exec.js" <<JS
const ba = /^ba/; let s = 0;
for (let i = 0; i < $N; i++) if (ba.exec('bacde')) s++;
console.log(s);
JS
cat >"$D/miss.js" <<JS
const ba = /^ba/; let s = 0;
for (let i = 0; i < $N; i++) if (ba.exec('xyzzy')) s++;
console.log(s);
JS
cat >"$D/replace.js" <<JS
const re = /a/g; let s = 0; const text = 'xaxaxaxaxaxaxaxaxaxa';
for (let i = 0; i < $(($N / 10)); i++) s += text.replace(re, 'b').length;
console.log(s);
JS
cat >"$D/literal.js" <<JS
let s = 0;
for (let i = 0; i < $N; i++) if (/\bba\b/.test('x')) s++;
console.log(s);
JS
instr() { /usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/instructions retired/ {print $1}'; }
base=$(instr "$O" "$D/empty.js")
for f in test exec miss replace literal; do
  n=$(instr "$O" "$D/$f.js")
  iters=$N; [ "$f" = replace ] && iters=$((N / 10))
  echo "$f $(( (n - base) / iters )) instr/op$([ "$f" = replace ] && echo ' (10 matches)')"
done
rm -rf "$D"
