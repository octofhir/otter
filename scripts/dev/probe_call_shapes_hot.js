// Call shapes inside functions called many times, so baseline feedback
// precedes optimization (node's emit / constructor helpers).
const fs = [(x) => x + 1, (x) => x + 2, (x) => x + 3, (x) => x + 4, (x) => x + 5, (x) => x + 6];
function applyMega(c, i) { const a = [c]; return Reflect.apply(fs[i % 6], null, a); }
function applyRest(f, ...args) { return Reflect.apply(f, null, args); }
class P { constructor(x) { this.x = x; } }
class Q { constructor(x) { this.x = x; this.y = 1; } }
const ks = [P, Q];
function newPoly(i) { return new ks[i & 1](i).x; }
function bench(name, f, n = 1e7) {
  for (let w = 0; w < 5; w++) f(n / 10);
  const t = performance.now(); f(n);
  console.log(name.padEnd(16), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns');
}
bench('apply mega', (n) => { let c = 0; for (let i = 0; i < n; i++) c = applyMega(c, i); return c; });
bench('apply rest', (n) => { let c = 0; for (let i = 0; i < n; i++) c = applyRest(fs[i % 6], c); return c; });
bench('new poly', (n) => { let c = 0; for (let i = 0; i < n; i++) c += newPoly(i); return c; }, 3e6);
