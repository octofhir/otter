// A runtime operation that throws inside a try block, with many values live
// across it: getters, instanceof of a non-callable, and a proxy trap.
let caught = 0;
const thrower = { get x() { throw new Error("getter"); } };
const proxy = new Proxy({}, { get() { throw new TypeError("trap"); } });
function work(n) {
  let a = 1, b = 2, c = 3, d = 4.5, e = "e", f = [1, 2], g = { h: 1 };
  let sum = 0;
  for (let i = 0; i < n; i++) {
    a += i; b ^= i; c = (c * 3) | 0; d += 0.5;
    try {
      if (i % 97 === 0) sum += thrower.x;
      else if (i % 89 === 0) sum += ({} instanceof (i & 1 ? 5 : null)) ? 1 : 0;
      else if (i % 83 === 0) sum += proxy.y;
      else sum += g.h;
    } catch (err) {
      caught++;
      sum += a + b + c + d + e.length + f.length + (err instanceof TypeError ? 10 : 20);
    }
  }
  return sum;
}
let total = 0;
for (let r = 0; r < 30; r++) total += work(2000);
console.log(total, caught);
