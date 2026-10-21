function sum(a) { let s = 0; for (let i = 0; i < a.length; i++) s = (s + a[i]) | 0; return s; }
const a = new Int32Array(1000).fill(3); let t = 0;
for (let k = 0; k < 20000; k++) t += sum(a);
console.log(t);
