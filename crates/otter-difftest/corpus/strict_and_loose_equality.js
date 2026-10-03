// Strict and loose equality in optimized code over every value class: int32
// and double numbers (NaN, -0), strings compared by content, BigInts, objects
// by identity, oddballs, and mixed pairs; fused branches and materialized
// booleans alike.
'use strict';
const o1 = {}, o2 = {};
const s1 = 'ab' + 'c', s2 = ['a', 'bc'].join('');
const values = [0, -0, 1, 1.5, NaN, -1, 2147483647, 1e300, s1, s2, 'abd', '',
  10n, 10n * 1n, 11n, o1, o2, undefined, null, true, false, Symbol.iterator];
function strict(a, b) { return a === b; }
function notStrict(a, b) { return a !== b; }
function strictBranch(a, b) { if (a === b) return 1; return 0; }
function vsUndefined(a) { return a === undefined ? 'u' : a !== null ? 'v' : 'n'; }
function loose(a, b) { return a == b; }
function looseInts(n) {
  let hits = 0;
  for (let i = 0; i < n; i++) { const x = (i * 7) | 0, y = (i & 3) | 0; if (x == y) hits++; if (x != 21) hits += 2; }
  return hits;
}
let out = [];
for (let round = 0; round < 400; round++) {
  out = [];
  for (const a of values) {
    let row = '';
    for (const b of values) {
      row += (strict(a, b) ? 1 : 0) + '' + (notStrict(a, b) ? 1 : 0) + strictBranch(a, b) + (loose(a, b) ? 1 : 0);
    }
    out.push(row + vsUndefined(a));
  }
}
console.log(out.join('\n'));
console.log(looseInts(100000));
