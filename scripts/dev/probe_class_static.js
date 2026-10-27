class A { static s(x) { return x; } }
class B { static x = 1; }
class E { }
Object.defineProperty(E, 's', { value: 1, writable: true, configurable: true });
class G { static s(x) { return x; } static t(x) { return x; } }
G.u = 3;
function bench(name, f, n) {
  f(n / 10);
  const t = performance.now();
  f(n);
  console.log(name.padEnd(20), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns/op');
}
bench('A.s method', (n) => { let c; for (let i = 0; i < n; i++) c = A.s; return c; }, 2e6);
bench('B.x field', (n) => { let c; for (let i = 0; i < n; i++) c = B.x; return c; }, 2e6);
bench('E.s defined', (n) => { let c; for (let i = 0; i < n; i++) c = E.s; return c; }, 2e6);
bench('G.u assigned', (n) => { let c; for (let i = 0; i < n; i++) c = G.u; return c; }, 2e6);
