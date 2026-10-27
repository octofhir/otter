// Per-call cost of call shapes common in node's library code.
function bench(name, f, n = 1e7) {
  f(n / 10);
  const t = performance.now();
  const r = f(n);
  console.log(name.padEnd(22), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns', typeof r);
}
const fs = [(x) => x + 1, (x) => x + 2, (x) => x + 3, (x) => x + 4, (x) => x + 5, (x) => x + 6];
function mono(x) { return x + 1; }
function noinline(x) { if (x === -1) { for (let i = 0; i < 100; i++) x += i * i; return x; } return x + 1; }
bench('mono direct', (n) => { let c = 0; for (let i = 0; i < n; i++) c = mono(c); return c; });
bench('poly 2', (n) => { let c = 0; for (let i = 0; i < n; i++) c = fs[i & 1](c); return c; });
bench('mega 6', (n) => { let c = 0; for (let i = 0; i < n; i++) c = fs[i % 6](c); return c; });
bench('fn.call mono', (n) => { let c = 0; for (let i = 0; i < n; i++) c = noinline.call(null, c); return c; });
bench('fn.call poly', (n) => { let c = 0; for (let i = 0; i < n; i++) c = fs[i & 1].call(null, c); return c; });
bench('Reflect.apply mega', (n) => { let c = 0; const a = [0]; for (let i = 0; i < n; i++) { a[0] = c; c = Reflect.apply(fs[i % 6], null, a); } return c; });
class P { constructor(x) { this.x = x; } }
class Q { constructor(x) { this.x = x; this.y = 1; } }
const ks = [P, Q];
bench('new mono', (n) => { let c = 0; for (let i = 0; i < n; i++) c += new P(i).x; return c; }, 3e6);
bench('new poly', (n) => { let c = 0; for (let i = 0; i < n; i++) c += new ks[i & 1](i).x; return c; }, 3e6);
