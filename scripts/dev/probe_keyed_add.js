// Keyed stores that add a string key to a fresh object (node's
// `events[type] = listener` on `{ __proto__: null }`).
function addNull(type, v) { const o = { __proto__: null }; o[type] = v; return o; }
function addPlain(type, v) { const o = {}; o[type] = v; return o; }
function overwrite(o, type, v) { o[type] = v; return o; }
const types = ['finish', 'close', 'error', 'data'];
function bench(name, f, n = 5e6) {
  for (let w = 0; w < 3; w++) f(n / 10);
  const t = performance.now(); f(n);
  console.log(name.padEnd(16), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns');
}
bench('add null-proto', (n) => { let c = 0; for (let i = 0; i < n; i++) c += addNull(types[i & 3], i) !== null; return c; });
bench('add plain', (n) => { let c = 0; for (let i = 0; i < n; i++) c += addPlain(types[i & 3], i) !== null; return c; });
const o = { __proto__: null, finish: 0, close: 0, error: 0, data: 0 };
bench('overwrite', (n) => { for (let i = 0; i < n; i++) overwrite(o, types[i & 3], i); return o; });
