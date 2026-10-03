// Exits from inside inlined bodies: a failed int32 speculation two frames
// deep, a throw from an inlined callee, and stack traces through it.
function add(a, b) { return a + b; }
class P {
  constructor(x) { this.x = x; }
  get2() { return add(this.x, this.x); }
}
function run(n) {
  let s = 0;
  const p = new P(3);
  for (let i = 0; i < n; i++) {
    if (i === n - 5) p.x = 2.5;
    s = s + add(i, 1) + p.get2();
  }
  return s;
}
console.log(run(300000));
function check(x) {
  if (x > 250000) throw new RangeError("too big " + x);
  return x & 1;
}
function scan(n) {
  let s = 0;
  for (let i = 0; i < n; i++) s += check(i);
  return s;
}
try {
  scan(300000);
} catch (e) {
  console.log(e.message, e.stack.split("\n").slice(0, 3).map((l) => l.trim().split(" ")[1]).join(","));
}
function outerAdd(a, b) { return add(a, b) | 0; }
function deep(n) {
  let s = 0;
  for (let i = 0; i < n; i++) s = (s + outerAdd(i, i === n - 2 ? "x" : 1)) | 0;
  return s;
}
console.log(deep(200000));
