// `f.call(this, x)` inside a function called many times (node's
// `Stream.call(this, opts)` shape), so baseline feedback precedes
// optimization.
function noinline(x) { if (x === -1) { for (let i = 0; i < 100; i++) x += i * i; return x; } return x + 1; }
function viaCall(c) { return noinline.call(null, c) + 0; }
const fs = [(x) => x + 1, (x) => x + 2];
function viaCallPoly(c, i) { return fs[i & 1].call(null, c) + 0; }
function bench(name, f, n = 1e7) {
  for (let w = 0; w < 5; w++) f(n / 10);
  const t = performance.now();
  const r = f(n);
  console.log(name.padEnd(16), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns', typeof r);
}
bench('call mono', (n) => { let c = 0; for (let i = 0; i < n; i++) c = viaCall(c); return c; });
bench('call poly', (n) => { let c = 0; for (let i = 0; i < n; i++) c = viaCallPoly(c, i); return c; });
