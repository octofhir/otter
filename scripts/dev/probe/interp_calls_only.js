// JS-to-JS calls in a loop, for an interpreter-only profile of the call path.
"use strict";
let sum = 0;
function f(a, b) { return a + b; }
function calls() { let s = 0; for (let i = 0; i < 1000; i++) s += f(i, 1); sum += s; }
const t = Date.now();
for (let i = 0; i < 20000; i++) calls();
console.log("calls", ((Date.now() - t) * 1e6 / 2e7).toFixed(1), "ns/iter", sum);
