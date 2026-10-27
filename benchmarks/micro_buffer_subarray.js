const b = Buffer.alloc(256, 7);
const u = new Uint8Array(256);
function bench(label, f) {
  const N = 2e6; let keep;
  for (let i = 0; i < 2e5; i++) keep = f(i);
  const t0 = process.hrtime.bigint();
  for (let i = 0; i < N; i++) keep = f(i);
  console.log(label, (Number(process.hrtime.bigint() - t0) / N).toFixed(1), 'ns', keep.length);
}
bench('buf.subarray', (i) => b.subarray(i & 15, 100));
bench('buf.slice', (i) => b.slice(i & 15, 100));
bench('u8.subarray', (i) => u.subarray(i & 15, 100));
bench('new FastBuffer-ish', (i) => Buffer.from(b.buffer, i & 15, 50));
