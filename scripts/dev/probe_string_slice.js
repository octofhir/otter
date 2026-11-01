// String.prototype.slice and substring of Latin-1, WTF-16, rope and boxed
// strings over every bound form from hot code, a null receiver, and many short
// results kept alive across the allocations of more.
'use strict';
const s = 'abcdefghijklmnopqrstuvwxyz';
const wide = 'αβγδεζηθικλμνξοπρστυφχψω';
const cons = (() => { let t = ''; for (let i = 0; i < 30; i++) t += String.fromCharCode(97 + (i % 26)); return t; })();
const bounds = [[], [3], [-4], [2, 9], [9, 2], [-8, -2], [0, 100], [undefined, 5], [5, undefined], [1.5, 7.9], [NaN], ['2', '6'], [-100, 3]];
function run(str) {
  const out = [];
  for (const b of bounds) out.push(str.slice(...b) + '|' + str.substring(...b));
  return out.join(',');
}
let r = [];
for (let i = 0; i < 3000; i++) r = [run(s), run(wide), run(cons), run(new String('boxed-string'))];
let threw = '';
try { String.prototype.slice.call(null, 1); } catch (e) { threw = e.constructor.name; }
const keep = []; for (let i = 0; i < 20000; i++) keep.push(cons.slice(i % 20, (i % 20) + 9));
console.log(r.join('\n') + '\n' + threw + ' ' + keep.length + ' ' + keep[19999] + ' ' + keep[777]);
