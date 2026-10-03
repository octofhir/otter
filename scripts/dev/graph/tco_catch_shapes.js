"use strict";
// Tail calls from a catch clause of a called, compiled activation: the
// caught throw leaves compiled code for the interpreter, which then makes
// the tail call. A million levels must run in bounded stack.
let calls = 0;
function viaCatch(n) {
  if (n === 0) { calls++; return 'done'; }
  try { throw null; } catch (e) { return viaCatch(n - 1); }
}
function caller(n) { return viaCatch(n) + '!'; }
for (let i = 0; i < 2000; i++) caller(5);
console.log(caller(1000000), calls);
