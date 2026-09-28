// Strict equality across every value kind, hot enough for the optimizing
// tier. Identical bits are equal except NaN; numbers compare by value across
// int32 and double encodings (including +0/-0); strings and BigInts compare
// by contents; every other cell compares by identity.
const o = {}, p = {};
const s1 = "ab", s2 = ["a", "b"].join(""), s3 = "ac";
const b1 = 10n, b2 = BigInt("10"), b3 = 11n;
const sym = Symbol("x");
const f = () => 1;
const values = [0, -0, 1, 1.0, 1.5, 2 ** 31, -1, NaN, Infinity, "", s1, s2, s3,
  b1, b2, b3, sym, Symbol("x"), null, undefined, true, false, o, p, f, [], 0.1 + 0.2, 0.3];
function eq(a, b) { return a === b; }
function ne(a, b) { return a !== b; }
let sig = "";
for (let round = 0; round < 400; round++) {
  let bits = 0, count = 0;
  for (let i = 0; i < values.length; i++) {
    for (let j = 0; j < values.length; j++) {
      if (eq(values[i], values[j])) { bits = (bits * 31 + i * 97 + j) | 0; count++; }
      if (ne(values[i], values[j]) === eq(values[i], values[j])) count += 1000;
    }
  }
  if (round === 399) sig = count + ":" + bits;
}
console.log(sig);
