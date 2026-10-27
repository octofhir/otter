// Named loads on functions whose [[Prototype]] was replaced.
class Base { static inherited() { return 1; } }
function F() {}
Object.defineProperty(F, 'own', { value() { return 2; }, writable: true, configurable: true });
Object.setPrototypeOf(F, Base);
function G() {}
G.own = function () { return 3; };
Object.setPrototypeOf(G, { viaObject() { return 4; } });
function bench(name, f, n) {
  f(n / 10);
  const t = performance.now();
  const r = f(n);
  console.log(name.padEnd(22), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns/op', typeof r);
}
bench('F.own (proto=class)', (n) => { let c; for (let i = 0; i < n; i++) c = F.own; return c; }, 2e6);
bench('F.inherited', (n) => { let c; for (let i = 0; i < n; i++) c = F.inherited; return c; }, 2e6);
bench('G.own (proto=obj)', (n) => { let c; for (let i = 0; i < n; i++) c = G.own; return c; }, 2e6);
bench('G.viaObject', (n) => { let c; for (let i = 0; i < n; i++) c = G.viaObject; return c; }, 2e6);
bench('Buffer.from load', (n) => { let c; for (let i = 0; i < n; i++) c = Buffer.from; return c; }, 2e6);
console.log(F.own(), F.inherited(), G.own(), G.viaObject());
