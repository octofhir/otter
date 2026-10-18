// f.apply(o, pair) in a hot loop, for a profile of the apply path alone.
"use strict";
let sum = 0;
function f(a, b) { sum += a + b; }
const o = {};
const pair = [1, 2];
function g() { for (let i = 0; i < 1000; i++) f.apply(o, pair); }
const t = Date.now();
for (let i = 0; i < (globalThis.REPS || 40000); i++) g();
console.log("apply", ((Date.now() - t) * 1e6 / ((globalThis.REPS || 40000) * 1000)).toFixed(1), "ns/call", sum);
