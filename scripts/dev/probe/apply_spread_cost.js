// f.apply with an array, f(...array) and Math.max.apply against a direct call.
"use strict";
let sum = 0;
function f(a, b) { sum += a + b; }
const o = {};
const pair = [1, 2];
const nums = Array.from({ length: 8 }, (_, i) => i * 3);
function time(name, g) {
  for (let i = 0; i < 200; i++) g();
  const t = Date.now();
  for (let i = 0; i < 3000; i++) g();
  const ms = Date.now() - t;
  console.log(name.padEnd(22), (ms * 1e6 / 3e6).toFixed(1), "ns/call");
}
time("direct", () => { for (let i = 0; i < 1000; i++) f(i, 1); });
time("apply fresh array", () => { for (let i = 0; i < 1000; i++) f.apply(o, [i, 1]); });
time("apply same array", () => { for (let i = 0; i < 1000; i++) f.apply(o, pair); });
time("spread", () => { for (let i = 0; i < 1000; i++) f(...pair); });
time("Math.max.apply", () => { for (let i = 0; i < 1000; i++) sum += Math.max.apply(null, nums); });
time("push.apply", () => { for (let i = 0; i < 1000; i++) { const a = []; Array.prototype.push.apply(a, nums); sum += a.length; } });
