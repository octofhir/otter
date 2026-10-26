// Per-call cost of builtins without a dedicated fast path, called from hot
// optimized code.
const o = { a: 1 };
const arr = [1, 2, 3, 4, 5, 6, 7, 8];
function bench(name, f) {
  for (let w = 0; w < 3; w++) f(1000);
  const n = 5000000, t = performance.now();
  const s = f(n);
  console.log(name.padEnd(16), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns', s);
}
bench('getPrototypeOf', (n) => { let s = 0; for (let i = 0; i < n; i++) s += Object.getPrototypeOf(o) === Object.prototype ? 1 : 0; return s; });
bench('hasOwnProperty', (n) => { let s = 0; for (let i = 0; i < n; i++) s += o.hasOwnProperty('a') ? 1 : 0; return s; });
bench('arr.indexOf', (n) => { let s = 0; for (let i = 0; i < n; i++) s += arr.indexOf(i & 7); return s; });
bench('Array.isArray', (n) => { let s = 0; for (let i = 0; i < n; i++) s += Array.isArray(arr) ? 1 : 0; return s; });
bench('String.fromCharCode', (n) => { let s = 0; for (let i = 0; i < n; i++) s += String.fromCharCode(65 + (i & 7)).length; return s; });
bench('Object.keys', (n) => { let s = 0; for (let i = 0; i < n; i++) s += Object.keys(o).length; return s; });
