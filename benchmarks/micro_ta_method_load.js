const u = new Uint8Array(64);
const b = Buffer.alloc(64);
function bench(label, f) {
  const N = 1e7; let keep;
  for (let i = 0; i < 1e5; i++) keep = f(i);
  const t0 = process.hrtime.bigint();
  for (let i = 0; i < N; i++) keep = f(i);
  console.log(label, (Number(process.hrtime.bigint() - t0) / N).toFixed(1), 'ns', typeof keep);
}
bench('u8.subarray load', () => u.subarray);
bench('buf.write load', () => b.write);
bench('buf.length', () => b.length);
bench('u8.byteLength', () => u.byteLength);
bench('buf[3]', (i) => b[i & 63]);
