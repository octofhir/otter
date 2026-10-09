// ToBoolean in optimized branches and `!` over every value kind: doubles
// (±0, NaN, fractions), strings (empty, flat, rope, sliced), objects,
// arrays, functions, natives, symbols, BigInts and the immediates.
function branch(v) { if (v) return 1; return 0; }
function not(v) { return !v; }
function both(v) { return (v ? 2 : 0) + (!v ? 1 : 0); }
const base = "abcdef";
const values = [
  0, 1, -1, 0.5, -0, NaN, Infinity, -Infinity, 1e-300, 2 ** 40,
  "", "x", base + base, base.slice(2, 2), base.slice(1, 4), "" + "",
  {}, [], [0], function () {}, () => 0, Math.max, parseInt, Symbol("s"),
  0n, 1n, -1n, 2n ** 70n, null, undefined, true, false, new Boolean(false), new String(""),
];
let acc = 0;
for (let round = 0; round < 3000; round++) {
  for (let i = 0; i < values.length; i++) {
    const v = values[i];
    acc = (acc * 7 + branch(v) * (i + 1) + (not(v) ? 3 : 5) + both(v)) | 0;
  }
}
console.log(acc);
