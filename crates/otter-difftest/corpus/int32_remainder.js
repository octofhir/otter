// Speculative Int32 `%` once hot: every divisor sign, powers of two, zero
// divisors (NaN), negative-zero results, and INT32_MIN must match the
// interpreter, before and after the speculation exits.

function rem(a, b) {
  return a % b;
}
function hot(n) {
  let s = 0;
  for (let i = 0; i < n; i++) s += rem(i, 7) + rem(i, 8) + rem(-i, 5) + rem(i, -3);
  return s;
}
console.log(hot(20000));

const MIN = -2147483648;
const MAX = 2147483647;
const cases = [
  [7, 3], [-7, 3], [7, -3], [-7, -3], [8, 4], [-8, 4], [5, 1], [-5, 1],
  [MIN, -1], [MIN, 1], [MIN, MIN], [MAX, MIN], [MIN, MAX], [MAX, 2], [MIN, 2],
  [0, 5], [5, 0], [-5, 0], [0, 0], [123456, 1024], [-123456, 1024],
];
for (const [a, b] of cases) {
  const r = rem(a, b);
  console.log(a, b, Object.is(r, -0) ? "-0" : r);
}

// A site whose operands stay Int32 but whose results include -0 and NaN
// after the speculation settled.
function mixed(n) {
  let zeros = 0;
  let nans = 0;
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const r = rem(i - n / 2, (i % 9) - 4);
    if (Number.isNaN(r)) nans++;
    else if (Object.is(r, -0)) zeros++;
    else sum += r;
  }
  return [zeros, nans, sum];
}
console.log(mixed(20000).join());
console.log(hot(20000));

// Doubles reaching a site first specialized to Int32.
function remAny(a, b) {
  return a % b;
}
let t = 0;
for (let i = 0; i < 20000; i++) t += remAny(i, 13);
console.log(t, remAny(7.5, 2), remAny(-7.5, 2), remAny(5, 2.5), remAny(1e10, 7));
