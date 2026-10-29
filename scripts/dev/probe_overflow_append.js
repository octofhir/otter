// Compiled named stores that append suffix (out-of-object) slots: fresh and
// reused receivers, slab growth under allocation pressure, a setter that
// appears on the prototype later, non-extensible receivers, shape mixes.
function fill(o, i) {
  o.a = i; o.b = i + 1; o.c = i + 2; o.d = i + 3; o.e = i + 4; o.f = i + 5;
  o.g = i + 6; o.h = i + 7; o.k = { i }; o.l = 'x' + (i & 7); o.m = i * 0.5; o.n = null;
  return o;
}
function P() {}
function run(n) {
  const keep = [];
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const o = fill(i & 1 ? new P() : {}, i);
    if (i % 97 === 0) keep.push(fill(o, i + 1));
    if (i % 5 === 0) fill(o, i + 2);
    if (i % 1013 === 0) Object.preventExtensions(o);
    sum += o.a + o.h + o.k.i + o.m;
    if (i === n / 2) {
      let seen = 0;
      Object.defineProperty(P.prototype, 'g', { set(v) { seen += v; }, get() { return -1; }, configurable: true });
      const q = fill(new P(), 3);
      keep.push(q.g, seen, Object.keys(q).length);
    }
    if (i % 4000 === 0) keep.push(new Array(2000).fill(i).length);
  }
  return [sum, keep.length, JSON.stringify(keep.slice(-12))];
}
function frozen() {
  'use strict';
  const r = [];
  for (let i = 0; i < 3000; i++) {
    const o = Object.preventExtensions({ a: 1 });
    try { fill(o, i); r.push('nothrow'); } catch (e) { r.push(e.constructor.name); }
  }
  return [r.length, r[0], r[2999]];
}
console.log(JSON.stringify(run(40000)));
console.log(JSON.stringify(frozen()));
