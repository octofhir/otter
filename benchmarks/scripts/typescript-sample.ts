// TypeScript sample — proves each runtime parses/strips TS. Types are
// erased; the workload is a typed matrix multiply (cache-ish access pattern).
const N: number = 64;

function makeMatrix(seed: number): Float64Array {
  const m = new Float64Array(N * N);
  let s = seed >>> 0;
  for (let i = 0; i < m.length; i++) {
    s = (s * 1103515245 + 12345) >>> 0;
    m[i] = (s & 0xffff) / 65536;
  }
  return m;
}

function matmul(a: Float64Array, b: Float64Array): Float64Array {
  const c = new Float64Array(N * N);
  for (let i = 0; i < N; i++) {
    for (let k = 0; k < N; k++) {
      const aik = a[i * N + k];
      for (let j = 0; j < N; j++) {
        c[i * N + j] += aik * b[k * N + j];
      }
    }
  }
  return c;
}

const a = makeMatrix(1);
const b = makeMatrix(2);
let acc = 0;
for (let r = 0; r < 8; r++) {
  const c = matmul(a, b);
  acc += c[0] + c[c.length - 1];
}
console.log(acc.toFixed(4));
