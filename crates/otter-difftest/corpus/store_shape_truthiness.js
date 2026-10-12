// Truthiness of unboxed numbers and proved objects, and named stores into
// receivers whose shape is proved, including aliases of a transitioned object.
const results = [];

// `x ? x : d` over int32 and double values, with zero, negative zero and NaN.
function pick(x, d) { return x ? x : d; }
function ints(n) { let s = 0; for (let i = -3; i < n; i++) s += pick(i | 0, 100); return s; }
function doubles(n) {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const x = (i % 5) * 0.5 - 1;          // -1, -0.5, 0, 0.5, 1
    s += pick(x, 10) + pick(-x * 0, 20) + pick(x / 0 - x / 0, 30);  // -0 and NaN
  }
  return s;
}
let total = 0;
for (let r = 0; r < 300; r++) total += ints(50) + doubles(50);
results.push(total, pick(0, 'zero'), pick(-0, 'negzero'), pick(NaN, 'nan'), pick(0.25, 'q'));

// Objects proved by their shape are truthy; a falsy non-object is not.
function P(x) { this.x = x; }
function either(p, q) { return (p ? p.x : -1) + (q ? q.x : -2); }
let objs = 0;
for (let i = 0; i < 20000; i++) objs += either(new P(i), i % 3 ? new P(1) : null);
results.push(objs);

// A transition through one parameter while another names the same object.
function grow(a, b) {
  const before = b.x;
  a.y = before + 1;       // a: shape {x} -> {x,y}
  return b.y + b.x;       // b is a; its old shape no longer holds
}
let aliased = 0;
for (let i = 0; i < 20000; i++) { const o = new P(i); aliased += grow(o, o); }
results.push(aliased);

// A store site seeing several shapes, its receiver proved to have one.
function Q(x) { this.q = x; this.x = x; }
function setZ(o, v) { o.z = v; return o.z + o.x; }
let poly = 0;
for (let i = 0; i < 20000; i++) {
  const o = i & 1 ? new P(i) : new Q(i);
  poly += setZ(o, i & 7);
  const p = new P(i);
  p.z = 1;
  poly += p.z;
}
results.push(poly);

// Fresh receivers built through chained stores keep every field.
function Vec(x, y, z) { this.x = x ? x : 0; this.y = y ? y : 0; this.z = z ? z : 0; }
function add(u, v) { return new Vec(u.x + v.x, u.y + v.y, u.z + v.z); }
let v = new Vec(0, 0, 0);
const d = new Vec(0.5, -0.25, 0);
for (let i = 0; i < 40000; i++) v = add(v, d);
results.push(v.x, v.y, v.z, Object.keys(v).join());

console.log(JSON.stringify(results));
