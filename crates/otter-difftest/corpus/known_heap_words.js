// Known heap words in optimized code: a property or context slot read twice
// reuses the first value only while nothing could have written it. Stores
// through another reference to the same object, writes by a called closure,
// loop-carried stores and a loop that swaps the array it indexes all must be
// observed.
'use strict';
function Box(v) { this.v = v; this.w = 0; }
function alias(a, b, n) {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const x = a.v;
    b.v = x + 1;          // a and b may be the same object
    t += a.v + b.w;       // must re-read a.v when a === b
  }
  return t;
}
function counterLoop(n) {
  let count = 0;
  const bump = () => { count += 2; };
  let t = 0;
  for (let i = 0; i < n; i++) {
    t += count;
    if (i % 3 === 0) bump(); // a call writes the captured slot
    t += count;
  }
  return t;
}
function swapArrays(n) {
  let current = [1, 2, 3, 4];
  const other = [10, 20, 30, 40];
  let t = 0;
  const swap = () => { const tmp = current; current = other.slice(); other[0]++; return tmp; };
  for (let i = 0; i < n; i++) {
    t += current[i & 3];
    if ((i & 7) === 7) swap();
    t += current[(i + 1) & 3];
  }
  return t;
}
function storeInLoop(o, n) {
  let t = 0;
  for (let i = 0; i < n; i++) { o.v = o.v + i; t += o.v; o.w = t & 255; }
  return t + o.w;
}
let out = [];
for (let r = 0; r < 300; r++) {
  const p = new Box(1), q = new Box(5);
  out = [alias(p, q, 50), alias(p, p, 50), counterLoop(60), swapArrays(64), storeInLoop(new Box(2), 40)];
}
console.log(out.join(' '));
