// `==` / `!=` at optimized sites over every operand pairing: identity,
// Numbers of either encoding, null/undefined, objects, strings with and
// without recorded atoms, BigInts, booleans, and objects that convert.
function eq(a, b) { return a == b; }
function ne(a, b) { return a != b; }
function branch(a, b) { if (a == b) return 1; return 0; }
const o = { k: 1 };
const p = { k: 1 };
let conversions = 0;
const conv = { valueOf() { conversions++; return 7; } };
const strConv = { toString() { conversions++; return "ab"; } };
const fn = function () {};
const sym = Symbol("s");
const big = 10n;
const values = [
  0, -0, 1, 7, 1.5, NaN, Infinity, 2 ** 31, -1,
  "", "0", "1", "7", "ab", "a" + "b", "ab".slice(0), "1.5",
  null, undefined, true, false,
  o, p, conv, strConv, fn, sym, big, 7n, 0n,
  [], [7], ["ab"],
];
function scan(values, salt) {
  let row = salt;
  for (let i = 0; i < values.length; i++) {
    for (let j = 0; j < values.length; j++) {
      const a = values[i], b = values[j];
      let r = 0;
      try {
        if (a == b) r += 1;
        if (a != b) r += 2;
        r += branch(a, b) * 4 + (eq(a, b) ? 16 : 0) + (ne(a, b) ? 32 : 0);
      } catch (e) {
        r = 8;
      }
      row = (row * 31 + r * (i + 1) + j) | 0;
    }
  }
  return row;
}
const out = [];
for (let round = 0; round < 300; round++) out.push(scan(values, round));
console.log(out.slice(-3).join(), conversions);
// A throwing conversion propagates out of the optimized site.
const bad = { valueOf() { throw new Error("no"); } };
let caught = 0;
for (let i = 0; i < 2000; i++) {
  try { eq(bad, i); } catch (e) { caught++; }
}
// Symbol against an object converting to it.
const holder = { [Symbol.toPrimitive]() { return sym; } };
let symHits = 0;
for (let i = 0; i < 2000; i++) if (eq(holder, sym)) symHits++;
console.log(caught, symHits);
