// Fixed-work loop for retired-instruction deltas: run with n=0 and n=1e6.
"use strict";
function main(n) { let s = 0; for (let i = 0; i < n; i++) s += i; return s; }
console.log(main(+(globalThis.process?.argv?.at(-1) ?? 0) || 0));
