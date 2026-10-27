function bench(label, f) {
  const N = 1e7;
  let s = 0;
  for (let i = 0; i < 1e5; i++) s += f(i);
  const t0 = process.hrtime.bigint();
  for (let i = 0; i < N; i++) s += f(i);
  console.log(label, (Number(process.hrtime.bigint() - t0) / N).toFixed(2), 'ns', s > 0);
}
bench('Math.cbrt', (i) => Math.cbrt(i));
bench('Number.isSafeInteger', (i) => Number.isSafeInteger(i) ? 1 : 0);
bench('String.fromCodePoint', (i) => String.fromCodePoint(65).length);
bench('Object.is', (i) => Object.is(i, 3) ? 1 : 2);
