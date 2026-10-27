const kA = Symbol('a'), kB = Symbol('b');
function C() { this.x = 1; this[kA] = 0; this[kB] = null; }
const o = new C();
function bench(label, f) {
  const N = 2e7; let keep;
  for (let i = 0; i < 2e5; i++) keep = f(i);
  const t0 = process.hrtime.bigint();
  for (let i = 0; i < N; i++) keep = f(i);
  console.log(label, (Number(process.hrtime.bigint() - t0) / N).toFixed(2), 'ns', typeof keep);
}
bench('this[kA] read', () => o[kA]);
bench('this[kA] write', (i) => { o[kA] = i; return 0; });
bench('new C (2 sym stores)', () => new C());
