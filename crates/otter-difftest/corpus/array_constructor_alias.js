// `new` of the original %Array% through an alias: lengths, non-number and
// invalid lengths, no arguments, and a site whose target changes.
var Vec = Array;
function make(n) { return new Vec(n); }
function none() { return new Vec(); }
let out = [];
let total = 0;
for (let i = 0; i < 20000; i++) {
  const a = make(i & 15);
  total += a.length + (Array.isArray(a) ? 1 : 0) + (Object.getPrototypeOf(a) === Array.prototype ? 1 : 0);
  if ((i & 15) > 0 && (0 in a)) total += 1000;
}
out.push(total);
let empty = 0;
for (let i = 0; i < 5000; i++) empty += none().length;
out.push(empty);
const odd = make("x");
out.push(odd.length, odd[0]);
const dbl = make(3.0);
out.push(dbl.length);
let errors = 0;
for (const bad of [-1, 1.5, 2 ** 32]) {
  try { make(bad); } catch (e) { if (e instanceof RangeError) errors++; }
}
out.push(errors);
out.push(make(2 ** 31 - 1 > 1e9 ? 7 : 0).length);
Vec = Object;
const o = make(5);
out.push(typeof o, Array.isArray(o));
Vec = Array;
out.push(make(4).length);
console.log(JSON.stringify(out));
