// Megamorphic named loads and stores in optimized code: dozens of receiver
// shapes through one site, inherited properties, getters and setters that
// allocate (collect) or throw, add-transitions into inline and slab storage,
// and stores that must not reach prototype objects.
'use strict';
const shapes = [];
for (let k = 0; k < 40; k++) {
  const o = {};
  for (let j = 0; j < k % 9; j++) o['p' + j] = j;
  o.v = k;
  shapes.push(o);
}
const base = { get g() { return this.v * 2; } };
const inherit = [];
for (let k = 0; k < 12; k++) { const o = Object.create(base); o['q' + k] = k; o.v = k + 100; inherit.push(o); }
let junk = [];
const getterAllocates = { get v() { junk = []; for (let i = 0; i < 50; i++) junk.push({ i }); return junk.length; } };
const getterThrows = { get v() { throw new Error('g'); } };
function readV(o) { return o.v; }
function readG(o) { return o.g; }
function writeW(o, x) { o.w = x; }
let sum = 0, caught = 0;
for (let r = 0; r < 1500; r++) {
  for (const o of shapes) { sum += readV(o); writeW(o, r); }
  for (const o of inherit) sum += readG(o);
  sum += readV(getterAllocates);
  try { readV(getterThrows); } catch (e) { caught++; }
  const fresh = []; for (let k = 0; k < 20; k++) { const o = {}; for (let j = 0; j < k; j++) o['z' + j] = j; writeW(o, k); fresh.push(o.w); }
  sum += fresh.reduce((a, b) => a + b, 0);
}
writeW(base, 9);
console.log(sum, caught, shapes[3].w, base.w, Object.keys(shapes[8]).join(','));
