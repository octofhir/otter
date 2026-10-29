// Dictionary-mode objects keyed by atom: lookups by runtime-built strings,
// keys never seen before, deletes that compact the table, integer-like and
// unicode keys, enumeration order, JSON, for-in, prototype fallbacks.
function make(n, prefix) {
  const o = {};
  for (let i = 0; i < n; i++) o[prefix + i] = i;
  return o;
}
function run() {
  const out = [];
  const d = make(300, 'k');
  out.push(Object.keys(d).length, d['k' + 7], d['k' + 299], d['nope' + Math.random()] === undefined);
  for (let i = 0; i < 300; i += 3) delete d['k' + i];
  out.push(Object.keys(d).length, d.k1, d.k3, 'k3' in d, d['k' + 298]);
  d['10'] = 'ten'; d['2'] = 'two'; d['é' + 'x'] = 'eacute'; d['\u{1f600}'] = 'smile';
  out.push(Object.keys(d).slice(0, 4).join(), d['éx'], d['\u{1f600}']);
  let count = 0, sum = 0;
  for (const k in d) { count++; if (typeof d[k] === 'number') sum += d[k]; }
  out.push(count, sum);
  const p = make(200, 'p');
  Object.setPrototypeOf(d, p);
  out.push(d.p5, d['p' + 199], 'p0' in d, Object.keys(d).length);
  out.push(JSON.stringify(make(130, 'j')).length);
  const big = Object.create(null);
  for (let i = 0; i < 5000; i++) big['w' + (i * 7919 % 5000)] = i;
  let hits = 0;
  for (let r = 0; r < 20; r++) for (let i = 0; i < 5000; i++) if (big['w' + i] !== undefined) hits++;
  out.push(hits, Object.keys(big)[0], Object.keys(big).length);
  const acc = make(150, 'a');
  Object.defineProperty(acc, 'a3', { get() { return 'got'; } });
  out.push(acc.a3, acc['a' + 3], Object.getOwnPropertyDescriptor(acc, 'a4').value);
  return out;
}
function writes() {
  'use strict';
  const out = [];
  const d = make(300, 'k');
  for (let r = 0; r < 50; r++) for (let i = 0; i < 300; i++) d['k' + i] = { r, i };
  out.push(d.k5.r, d['k' + 299].i);
  Object.defineProperty(d, 'k7', { writable: false });
  try { d['k' + 7] = 1; out.push('nothrow'); } catch (e) { out.push(e.constructor.name, d.k7.i); }
  let seen = 0;
  Object.defineProperty(d, 'k8', { set(v) { seen += v; }, get() { return seen; } });
  for (let i = 0; i < 100; i++) d['k' + 8] = 2;
  out.push(d.k8);
  const f = make(200, 'f');
  Object.freeze(f);
  try { f['f' + 3] = 9; out.push('nothrow'); } catch (e) { out.push(e.constructor.name, f.f3); }
  const proto = make(200, 'm');
  function C() {}
  C.prototype = proto;
  const inst = new C();
  for (let i = 0; i < 200; i++) { proto['m' + i] = i * 2; }
  out.push(inst.m10, inst['m' + 199]);
  for (let r = 0; r < 3; r++) for (let i = 0; i < 200; i++) inst['m' + i] = -i;
  out.push(inst.m10, proto.m10, Object.keys(inst).length);
  const g = make(400, 'g');
  for (let r = 0; r < 200; r++) { g['g' + (r % 400)] = 'x'.repeat(r % 7) + r; if (r % 50 === 0) new Array(10000).fill(r); }
  out.push(g.g0, g.g199, Object.values(g).filter((v) => typeof v === 'string').length);
  return out;
}
console.log(JSON.stringify(run()));
console.log(JSON.stringify(writes()));
