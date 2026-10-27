'use strict';
// Strict tail calls after optimization: unbounded depth must not overflow.
function count(n, acc) { return n === 0 ? acc : count(n - 1, acc + 1); }
function even(n) { return n === 0 ? true : odd(n - 1); }
function odd(n) { return n === 0 ? false : even(n - 1); }
function grow(n, a) { return n === 0 ? a : grow3(n - 1, a, 1, 2); }
function grow3(n, a, b, c) { return grow(n, a + b + c - 2); }
let s = 0;
for (let i = 0; i < 20000; i++) s += count(50, 0) + (even(40) ? 1 : 0) + grow(20, 0);
console.log(s, count(1e6, 0), even(1e6 + 1), grow(5e5, 0));
try { (function deep(n) { return n === 0 ? 0 : 1 + deep(n - 1); })(1e7); } catch (e) { console.log(e instanceof RangeError); }
