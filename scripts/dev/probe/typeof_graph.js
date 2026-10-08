// typeof tests compiled by the optimizing tier: every kind, negated forms, inlined predicates.
function isNumber(n) { return typeof n === "number"; }
function notString(s) { return typeof s !== "string"; }
const values = [1, 1.5, -0, NaN, "s", "", true, false, null, undefined, {}, [], function () {},
  class {}, Symbol("x"), 10n, new Proxy({}, {}), new Proxy(function () {}, {}), Math.max, /r/, new Date(0)];
const kinds = ["undefined", "object", "boolean", "number", "bigint", "string", "symbol", "function"];
function classify(v) {
  let bits = 0;
  if (typeof v === "undefined") bits |= 1;
  if (typeof v === "object") bits |= 2;
  if (typeof v === "boolean") bits |= 4;
  if (typeof v === "number") bits |= 8;
  if (typeof v === "bigint") bits |= 16;
  if (typeof v === "string") bits |= 32;
  if (typeof v === "symbol") bits |= 64;
  if (typeof v === "function") bits |= 128;
  if (typeof v !== "object") bits |= 256;
  if (isNumber(v)) bits |= 512;
  if (notString(v)) bits |= 1024;
  return bits;
}
let sum = 0;
const out = [];
for (let round = 0; round < 20000; round++) {
  for (let i = 0; i < values.length; i++) sum = (sum + classify(values[i]) * (i + 1)) | 0;
}
for (const v of values) out.push(classify(v));
console.log(sum, out.join(","), kinds.length);
