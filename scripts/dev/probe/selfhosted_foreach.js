// A forEach written in JavaScript against the native one: what a
// self-hosted builtin would cost per element.
"use strict";
const arr = Array.from({ length: 1000 }, (_, i) => i);
let sum = 0;
function cb(x) { sum += x; }
function jsForEach(callbackfn, thisArg = undefined) {
  const O = Object(this);
  const len = O.length >>> 0;
  if (typeof callbackfn !== "function") throw new TypeError("not a function");
  for (let k = 0; k < len; k++) {
    if (k in O) callbackfn(O[k], k, O);
  }
}
function time(name, f) {
  for (let i = 0; i < 200; i++) f();
  const t = Date.now();
  for (let i = 0; i < 5000; i++) f();
  const ms = Date.now() - t;
  console.log(name, ms, "ms", (ms * 1e6 / 5e6).toFixed(1), "ns/elem");
}
time("native forEach", () => arr.forEach(cb));
time("js forEach", () => jsForEach.call(arr, cb));
time("js forEach, no in", () => { const O = arr; for (let k = 0; k < O.length; k++) cb(O[k], k, O); });
