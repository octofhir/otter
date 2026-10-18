// Fixed-work `Function.prototype.apply` calls for retired-instruction deltas.
"use strict";
function f(a, b) { return a + b; }
function main(n) { const args = [1, 2]; let s = 0; for (let i = 0; i < n; i++) s += f.apply(undefined, args); return s; }
console.log(main(+(globalThis.process?.argv?.at(-1) ?? 0) || 0));
