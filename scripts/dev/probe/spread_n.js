// Fixed-work spread calls for retired-instruction deltas: n=0 vs n=1e6.
"use strict";
function f(a, b) { return a + b; }
function main(n) { const args = [1, 2]; let s = 0; for (let i = 0; i < n; i++) s += f(...args); return s; }
console.log(main(+(globalThis.process?.argv?.at(-1) ?? 0) || 0));
