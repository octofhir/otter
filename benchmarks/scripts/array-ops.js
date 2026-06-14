// Array higher-order ops: map / filter / reduce / forEach over large arrays.
const N = 200_000;
const base = new Array(N);
for (let i = 0; i < N; i++) base[i] = i;

let acc = 0;
for (let r = 0; r < 12; r++) {
  const mapped = base.map((x) => x * 2 + 1);
  const filtered = mapped.filter((x) => (x % 3) === 0);
  const sum = filtered.reduce((a, b) => a + b, 0);
  acc += sum % 1_000_000;
  let f = 0;
  base.forEach((x) => { f += x & 7; });
  acc += f % 1000;
}
console.log(acc);
