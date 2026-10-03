// Inlined plain and method calls: values, deopts inside the callee, throws.
function add(a, b) { return a + b; }
function sq(x) { return x * x; }
function pick(a, b) { if (a > b) return a; return b; }
class P {
  constructor(x) { this.x = x; }
  get2() { return this.x * 2; }
  scaled(k) { return add(this.x, k) * 2; }
}
function run(n, p) {
  let s = 0;
  for (let i = 0; i < n; i++) {
    s = (s + add(i, 1) + sq(i & 15) + p.get2() + pick(i & 7, 3) + p.scaled(i & 3)) | 0;
  }
  return s;
}
let t = 0;
const p = new P(3);
for (let r = 0; r < 60; r++) t = (t + run(20000, p)) | 0;
// A double flowing into the inlined add deopts inside the callee.
t = (t + run(10, new P(1.5))) | 0;
console.log(t, add("a", 1), add(0.5, 0.25));
