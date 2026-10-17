// Interpreter cost of simple statements, calls and property access.
"use strict";
const REPS = 200;
let sum = 0;
function f(a, b) { return a + b; }
const obj = { x: 1, y: 2 };
const arr = [1, 2, 3, 4];
function loopOnly() { let s = 0; for (let i = 0; i < 1000; i++) s += i; sum += s; }
function calls() { let s = 0; for (let i = 0; i < 1000; i++) s += f(i, 1); sum += s; }
function props() { let s = 0; for (let i = 0; i < 1000; i++) s += obj.x + obj.y; sum += s; }
function elems() { let s = 0; for (let i = 0; i < 1000; i++) s += arr[i & 3]; sum += s; }
function allocs() { let s = 0; for (let i = 0; i < 1000; i++) { const o = { a: i }; s += o.a; } sum += s; }
function time(name, g) {
  const t = Date.now();
  for (let i = 0; i < REPS; i++) g();
  const ms = Date.now() - t;
  console.log(name.padEnd(8), (ms * 1e6 / (REPS * 1000)).toFixed(1), "ns/iter");
}
time("loop", loopOnly);
time("calls", calls);
time("props", props);
time("elems", elems);
time("allocs", allocs);
