"use strict";
// Deep proper tail calls in every shape the span rules distinguish: same
// arity, shrinking, growing past the caller's span, a concise arrow body,
// mutual recursion, and a constructing caller that must call ordinarily.
function same(n, acc) { return n === 0 ? acc : same(n - 1, acc + 1); }
function shrink(n, a, b, c) { return n === 0 ? a : shrinkTo(n - 1, a + 1); }
function shrinkTo(n, a) { return shrink(n, a, 0, 0); }
function grow(n) { return n === 0 ? 0 : grown(n - 1, 1, 2, 3, 4, 5); }
function grown(n, a, b, c, d, e) { return grow(n) + 0 === 0 ? a + b + c + d + e - 15 : 0; }
const arrow = (n, acc) => n === 0 ? acc : arrow(n - 1, acc + 2);
function even(n) { return n === 0 ? true : odd(n - 1); }
function odd(n) { return n === 0 ? false : even(n - 1); }
function growChain(n) { return n === 0 ? 'done' : growChain2(n - 1, n, n, n, n, n, n); }
function growChain2(n, a, b, c, d, e, f) { return growChain(n); }
function Ctor(n) { if (n > 0) return helper(n); }
function helper(n) { return n; }
const N = 1000000;
console.log(same(N, 0));
console.log(shrink(N, 0, 0, 0));
console.log(arrow(N, 0));
console.log(even(N), odd(N + 1));
console.log(growChain(N));
let built = 0;
for (let i = 0; i < 100000; i++) if (new Ctor(i) instanceof Ctor) built++;
console.log(built);
try { console.log(grow(10)); } catch (e) { console.log(e.name); }
