// Inlined callees read their own captured variables through the closure the
// call guard proved: distinct closures of one function see distinct cells,
// and writes through a sibling closure are visible to the inlined read.
function makeScaler(factor) {
  let offset = 0;
  return {
    apply: function (x) { return x * factor + offset; },
    bump: function () { offset++; },
  };
}

const a = makeScaler(2);
const b = makeScaler(3);

function run(s, n) {
  let total = 0;
  for (let i = 0; i < n; i++) {
    total += s.apply(i & 15);
    if ((i & 1023) === 0) s.bump();
  }
  return total;
}

let out = 0;
for (let round = 0; round < 30; round++) {
  out += run(a, 5000);
  out += run(b, 5000);
}
console.log(out, a.apply(10), b.apply(10));

function makeAdder(k) {
  return function add(x) { return x + k; };
}
const add5 = makeAdder(5);
const add7 = makeAdder(7);
function sumWith(f, n) {
  let s = 0;
  for (let i = 0; i < n; i++) s = (s + f(i)) | 0;
  return s;
}
let t = 0;
for (let r = 0; r < 40; r++) t = (t + sumWith(add5, 3000) + sumWith(add7, 3000)) | 0;
console.log(t);
