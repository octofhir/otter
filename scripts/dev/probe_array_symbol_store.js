// A symbol-keyed property added to a fresh array (EventEmitter's
// `existing[kEmitting] = 0` when an event gets its second listener).
const kEmitting = Symbol('kEmitting');
function pair(a, b) { const existing = [a, b]; existing[kEmitting] = 0; return existing; }
function bench(name, f, n = 5e6) {
  for (let w = 0; w < 3; w++) f(n / 10);
  const t = performance.now(); f(n);
  console.log(name.padEnd(16), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns');
}
bench('array sym add', (n) => { let c = 0; for (let i = 0; i < n; i++) c += pair(i, i)[kEmitting]; return c; });
bench('array str add', (n) => { let c = 0; for (let i = 0; i < n; i++) { const a = [i, i]; a.warned = 0; c += a.warned; } return c; });
