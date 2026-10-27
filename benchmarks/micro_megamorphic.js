// Megamorphic named load/store over objects of many shapes.
const makers = [];
for (let k = 0; k < 12; k++) {
  const fields = Array.from({ length: k + 1 }, (_, j) => 'f' + j);
  makers.push(new Function('x', `this.kind = x; ${fields.map((f) => `this.${f} = 0;`).join(' ')} this.parent = null;`));
}
const objs = [];
for (let i = 0; i < 1200; i++) objs.push(new makers[i % 12](i));
function storeAll(v) { for (let i = 0; i < objs.length; i++) objs[i].parent = v; }
function loadAll() { let s = 0; for (let i = 0; i < objs.length; i++) s += objs[i].kind; return s; }
function keyedAll(k) { let s = 0; for (let i = 0; i < objs.length; i++) s += objs[i][k]; return s; }
function time(label, fn) {
  for (let r = 0; r < 300; r++) fn(r);
  const n = 5000, t = process.hrtime.bigint();
  for (let r = 0; r < n; r++) fn(r);
  console.log(label.padEnd(10), (Number(process.hrtime.bigint() - t) / (n * objs.length)).toFixed(2), 'ns');
}
time('store', (r) => storeAll(r));
time('load', () => loadAll());
time('keyed', () => keyedAll('kind'));
