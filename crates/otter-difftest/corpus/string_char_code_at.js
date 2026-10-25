// `charCodeAt` over every string representation the optimizing tier meets:
// short and long Latin-1 and UTF-16 strings, ropes and slices, indices out of
// range, negative, fractional and missing; a hot loop reading `length` of
// strings only, then of an array; and the builtin replaced on
// `String.prototype` while hot code reads it.
function sum(s, n) {
  let total = 0;
  for (let i = 0; i < n; i++) total = (total + s.charCodeAt(i)) | 0;
  return total;
}
function probe(s, i) { return s.charCodeAt(i); }
const latin = "hello, world";
const longLatin = "abcdefghijklmnopqrstuvwxyz".repeat(20);
const wide = "héllo wörld ✓ ok";
const longWide = "αβγδεζηθικλμνξοπ".repeat(20);
let out = [];
for (let k = 0; k < 4000; k++) {
  out[0] = sum(latin, latin.length);
  out[1] = sum(longLatin, longLatin.length);
  out[2] = sum(wide, wide.length);
  out[3] = sum(longWide, longWide.length);
  out[4] = probe(latin, k % 12);
}
const rope = longLatin + longWide;
out.push(sum(rope, rope.length));
const slice = longWide.substring(5, 200);
out.push(sum(slice, slice.length));
out.push(String(probe(latin, 99)), String(probe(latin, -1)), probe(latin, 1.5), probe(latin));
out.push(String(sum(latin, latin.length + 1)));
function hash(s) {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return h;
}
for (let k = 0; k < 3000; k++) out[5] = hash(k & 1 ? longLatin : rope);
const fake = { length: 3, charCodeAt(i) { return 7 * i; } };
out.push(hash(fake));
const builtin = String.prototype.charCodeAt;
String.prototype.charCodeAt = function (i) { return i; };
out.push(hash(latin), sum(longLatin, 30));
String.prototype.charCodeAt = builtin;
out.push(hash(latin), sum(longLatin, 30));
console.log(JSON.stringify(out));
