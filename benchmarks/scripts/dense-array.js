// Dense array indexed reads/writes, append/pop, and length checks.
const N = 80000;
const a = new Array(N);
for (let i = 0; i < N; i++) a[i] = (i * 13) & 1023;

let acc = 0;
for (let r = 0; r < 35; r++) {
  for (let i = 0; i < N; i++) {
    const v = a[i];
    a[i] = (v + i + r) & 2047;
    acc = (acc + a[i]) | 0;
  }
  for (let i = 0; i < 2000; i++) a.push((i ^ r) & 255);
  for (let i = 0; i < 2000; i++) acc = (acc + a.pop()) | 0;
  acc = (acc + a.length) | 0;
}
console.log(acc >>> 0);
