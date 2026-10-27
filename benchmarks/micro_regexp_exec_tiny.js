// Fixed cost of one exec: a start-anchored literal over short subjects,
// matching and missing, and a no-capture literal search.
const ba = /^ba/, eq = /=/;
const subjects = ['bacde', 'xyzzy', 'bar12', 'qq'];
function bench(label, fn) {
  for (let i = 0; i < 2e5; i++) fn(i);
  const n = 5e6, t = process.hrtime.bigint();
  let s = 0;
  for (let i = 0; i < n; i++) s += fn(i) ? 1 : 0;
  console.log(label.padEnd(10), (Number(process.hrtime.bigint() - t) / n).toFixed(1), 'ns', s);
}
bench('hit', () => ba.exec('bacde'));
bench('miss', () => ba.exec('xyzzy'));
bench('mixed', (i) => ba.exec(subjects[i & 3]));
bench('eq', () => eq.exec('key=value'));
bench('test', () => ba.test('bacde'));
