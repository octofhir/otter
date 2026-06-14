// Typed array numeric kernels — Float64Array/Int32Array element access.
const N = 1 << 16;
const a = new Float64Array(N);
const b = new Float64Array(N);
const idx = new Int32Array(N);
for (let i = 0; i < N; i++) {
  a[i] = Math.sin(i) * 100;
  b[i] = Math.cos(i) * 100;
  idx[i] = (i * 2654435761) & (N - 1);
}
let acc = 0;
for (let r = 0; r < 25; r++) {
  let dot = 0;
  for (let i = 0; i < N; i++) dot += a[i] * b[i];
  // gather/scatter
  for (let i = 0; i < N; i++) a[i] = b[idx[i]] * 0.5 + a[i] * 0.5;
  acc += dot;
}
console.log(acc.toFixed(3));
