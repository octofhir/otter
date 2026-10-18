// Cost of f.call / f.apply against a direct call, hot (compiled callers).
"use strict";
let sum = 0;
function f(a, b) { sum += a + b; }
const o = {};
function direct() { for (let i = 0; i < 1000; i++) f(i, 1); }
function viaCall() { for (let i = 0; i < 1000; i++) f.call(o, i, 1); }
function viaApply() { for (let i = 0; i < 1000; i++) f.apply(o, [i, 1]); }
function forward() { return f.apply(this, arguments); }
function viaForward() { for (let i = 0; i < 1000; i++) forward.call(o, i, 1); }
function time(name, g) {
  for (let i = 0; i < 200; i++) g();
  const t = Date.now();
  for (let i = 0; i < (globalThis.REPS || 5000); i++) g();
  const ms = Date.now() - t;
  console.log(name, ms, "ms", (ms * 1e6 / ((globalThis.REPS || 5000) * 1000)).toFixed(1), "ns/call");
}
time("direct", direct);
time("call", viaCall);
time("apply", viaApply);
time("forward apply(arguments)", viaForward);
