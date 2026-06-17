const N = 2_000_000;
const arr = new Array(N);

function make(i) {
  return {
    a: i,
    b: i + 1,
    c: i + 2,
    d: i + 3,
    e: i + 4,
    f: i + 5,
    g: i + 6,
  };
}

for (let i = 0; i < N; i++) arr[i] = make(i);

let sum = 0;
for (let i = 0; i < N; i++) {
  const o = arr[i];
  sum += o.a + o.g;
}

console.log(sum);
