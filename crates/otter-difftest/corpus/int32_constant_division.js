// Integer remainders and quotients by constant divisors in hot loops: the
// optimizing tier forms them without a divide, and must answer exactly what
// the interpreter answers, -0 and inexact quotients included.
'use strict';
const dividends = [0, 1, -1, 2, -2, 3, -3, 7, -7, 8, -8, 100, -100, 1000, -1000, 65535, -65536,
  2147483647, -2147483647, -2147483648, 123456789, -987654321, 1073741824, -1073741824];
function mods(n) {
  return [n % 2, n % -2, n % 3, n % -3, n % 4, n % 7, n % -7, n % 8, n % 10, n % 1000, n % 65537,
    n % 2147483647, n % -2147483648, n % 1, n % -1, n % 1024, n % 641];
}
function divs(n) {
  return [n / 2, n / -2, n / 3, n / -3, n / 4, n / -8, n / 7, n / 10, n / 1000, n / -1000,
    n / 65537, n / 2147483647, n / -2147483648];
}
let acc = 0;
let minusZero = 0;
const seen = [];
for (let i = 0; i < 120000; i++) {
  const n = dividends[i % dividends.length] | 0;
  const m = mods(n);
  const d = divs(n);
  for (const v of m) {
    if (Object.is(v, -0)) minusZero++;
    acc = (acc + v) % 1000003;
  }
  for (const v of d) {
    if (Object.is(v, -0)) minusZero++;
    acc = (acc + Math.floor(v * 1000)) % 1000003;
  }
  if (i < dividends.length) seen.push(m.join(','), d.map((v) => v.toFixed(6)).join(','));
}
console.log(acc, minusZero);
console.log(seen.join('\n'));
