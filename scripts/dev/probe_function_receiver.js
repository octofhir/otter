// Named loads and method calls on function receivers (statics).
function F() {}
F.k = 3;
Object.defineProperty(F, 'm', { value(x) { return x + 1; }, writable: true, configurable: true });
class C { static s(x) { return x + 2; } }
function bench(name, f, n) {
  f(n / 10);
  const t = performance.now();
  const r = f(n);
  console.log(name.padEnd(20), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns/op', r);
}
bench('F.k load', (n) => { let c = 0; for (let i = 0; i < n; i++) c += F.k; return c; }, 5e6);
bench('F.m() call', (n) => { let c = 0; for (let i = 0; i < n; i++) c = F.m(c); return c; }, 5e6);
bench('C.s() static', (n) => { let c = 0; for (let i = 0; i < n; i++) c = C.s(c); return c; }, 5e6);
bench('Math.max', (n) => { let c = 0; for (let i = 0; i < n; i++) c = Math.max(c, i); return c; }, 5e6);
bench('Array.isArray', (n) => { let c = 0; const a = []; for (let i = 0; i < n; i++) c += Array.isArray(a) ? 1 : 0; return c; }, 5e6);
bench('F.length', (n) => { let c = 0; for (let i = 0; i < n; i++) c += F.length; return c; }, 5e6);
bench('F.name', (n) => { let c = 0; for (let i = 0; i < n; i++) c += F.name.length; return c; }, 5e6);
