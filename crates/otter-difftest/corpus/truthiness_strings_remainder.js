// Logical not, ToBoolean, string constants and float remainder in
// optimized code, over every value class.
'use strict';
const values = [0, -0, 1, -1, 0.5, NaN, '', 'a', null, undefined, true, false, {}, [], 0n, 1n, Symbol.iterator];
function nots(v) { return !v; }
function bools(v) { return v ? 'T' : 'F'; }
function strs(i) { return i % 2 ? 'odd' : 'even'; }
function rem(a, b) { return a % b; }
let out = '';
for (let r = 0; r < 2000; r++) {
  out = '';
  for (const v of values) out += (nots(v) ? 1 : 0) + bools(v) + (!nots(v) ? 'y' : 'n');
  for (let i = 0; i < 6; i++) out += strs(i);
  for (const [a, b] of [[7.5, 2], [-7.5, 2], [5, 0], [1e300, 3.25], [-0, 1], [Infinity, 2], [3, Infinity]]) out += ',' + rem(a, b);
}
console.log(out);
