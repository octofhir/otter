// `f.call(this, x)` with one monomorphic non-inlinable target, long enough
// to sample.
function noinline(x) { if (x === -1) { for (let i = 0; i < 100; i++) x += i * i; return x; } return x + 1; }
function run(n) { let c = 0; for (let i = 0; i < n; i++) c = noinline.call(null, c); return c; }
run(1e5);
const t = performance.now();
const r = run(Number(process.argv[2] || 1e8));
console.log(((performance.now() - t) * 1e6 / Number(process.argv[2] || 1e8)).toFixed(1), 'ns', r);
