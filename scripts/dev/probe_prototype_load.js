// `F.prototype` loads on functions and classes from optimized code.
function F() {}
class C {}
function bench(name, f, n) {
  f(n / 10);
  const t = performance.now();
  f(n);
  console.log(name.padEnd(16), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns/op');
}
bench('F.prototype', (n) => { let c; for (let i = 0; i < n; i++) c = F.prototype; return c; }, 2e6);
bench('C.prototype', (n) => { let c; for (let i = 0; i < n; i++) c = C.prototype; return c; }, 2e6);
bench('F.name', (n) => { let c; for (let i = 0; i < n; i++) c = F.name; return c; }, 2e6);
bench('F.length', (n) => { let c; for (let i = 0; i < n; i++) c = F.length; return c; }, 2e6);
