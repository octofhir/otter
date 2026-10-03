// Graph-tier element access: tagged arrays storing heap cells (young values
// into an old array, so the slab barrier must remember them across minor
// collections), holes read as undefined, packed doubles, typed views, strict
// stores, out-of-bounds reads and index kinds.
'use strict';
const keep = [];
for (let i = 0; i < 64; i++) keep.push({ i });
function fillObjects(a, n, round) {
  for (let i = 0; i < n; i++) a[i] = { v: i + round, s: 'x' + i };
}
function sumObjects(a, n) {
  let t = 0;
  for (let i = 0; i < n; i++) t += a[i].v + a[i].s.length;
  return t;
}
function holes(a) {
  let u = 0;
  for (let i = 0; i < a.length; i++) if (a[i] === undefined) u++;
  return u;
}
function dbl(a, n) {
  let t = 0;
  for (let i = 0; i < n; i++) { a[i] = a[i] * 1.5 + 0.25; t += a[i]; }
  return t;
}
function oob(a, n) {
  let t = 0;
  for (let i = 0; i <= n; i++) { const v = a[i]; t += v === undefined ? 1000 : v; }
  return t;
}
const objs = keep.slice();
let acc = 0;
for (let round = 0; round < 400; round++) {
  fillObjects(objs, 64, round);
  // Allocation churn so minor collections run between fills and reads.
  let junk = [];
  for (let k = 0; k < 200; k++) junk.push({ k, t: [k, k + 1] });
  acc += sumObjects(objs, 64) + junk.length;
}
console.log('objects', acc);
const sparse = [1, , 3, , 5, 6, , 8];
let h = 0;
for (let r = 0; r < 3000; r++) h += holes(sparse);
console.log('holes', h);
const d = [0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5];
let ds = 0;
for (let r = 0; r < 2000; r++) ds += dbl(d, d.length);
console.log('doubles', ds, d.join(','));
const ints = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
let o = 0;
for (let r = 0; r < 3000; r++) o += oob(ints, ints.length);
console.log('oob', o);
const u32 = new Uint32Array(16);
for (let r = 0; r < 3000; r++) for (let i = 0; i < 16; i++) u32[i] = (u32[i] * 2654435761 + i) >>> 0;
console.log('u32', u32.join(','));
const f32 = new Float32Array(8), c8 = new Uint8ClampedArray(8);
for (let r = 0; r < 3000; r++) for (let i = 0; i < 8; i++) { f32[i] = f32[i] / 3 + i * 1.1; c8[i] = f32[i] * 37.5 - 20; }
console.log('f32', f32.join(','), c8.join(','));
