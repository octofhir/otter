// Named stores through feedback programs in optimized code: constructors
// adding fields (shape transitions, inline and slab storage), polymorphic
// receivers, existing-slot writes, stores into objects whose storage is
// full (must leave), frozen and prototype receivers, and objects whose
// prototype gains a setter mid-run.
'use strict';
function Point(x, y) { this.x = x; this.y = y; this.z = x + y; }
function Wide(n) { for (let i = 0; i < n; i++) this['f' + i] = i; this.tail = n; }
function setV(o, v) { o.v = v; return o; }
function fill(o) { o.a = 1; o.b = 2; o.c = 3; o.d = 4; o.e = 5; o.f = 6; o.g = 7; o.h = 8; return o; }
let log = [];
let t = 0;
for (let r = 0; r < 4000; r++) {
  const p = new Point(r, 1);
  t += p.z;
  const w = new Wide(r % 7);
  t += w.tail;
  const objs = [{}, { v: 1 }, { q: 1 }, Object.freeze({}), Object.create({ set v(x) { log.push(x); } })];
  for (const o of objs) {
    try { setV(o, r); } catch (e) { log.push('E'); }
    t += o.v === r ? 1 : 0;
  }
  const f = fill({});
  t += f.h;
  if (r === 2000) Object.defineProperty(Object.prototype, 'h', { set(x) { log.push('h' + x); }, configurable: true });
}
console.log(t, log.length, log.slice(0, 5).join(','));
