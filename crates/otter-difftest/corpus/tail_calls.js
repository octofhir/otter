"use strict";
// Strict tail calls in every frame shape: the same span, a shrinking span,
// a span the tail callee outgrows, a concise arrow body, mutual recursion,
// non-bytecode callees, a constructing caller and a throw from deep inside a
// chain. Depths stay inside an ordinary call stack so any engine agrees.
function same(n, acc) { return n === 0 ? acc : same(n - 1, acc + n); }
function shrink(n, a, b, c) { return n === 0 ? a + b + c : shrinkTo(n - 1, a + 1); }
function shrinkTo(n, a) { return shrink(n, a, 1, 2); }
function grow(n) { return n === 0 ? 0 : grown(n - 1, n, 2, 3, 4, 5); }
function grown(n, a, b, c, d, e) { return n < 0 ? a + b + c + d + e : grow(n); }
const arrow = (n, acc) => n === 0 ? acc : arrow(n - 1, acc ^ n);
function even(n) { return n === 0 ? 1 : odd(n - 1); }
function odd(n) { return n === 0 ? 0 : even(n - 1); }
function toNative(a, b) { return Math.max(a, b); }
const bound = function (x) { return x + this.k; }.bind({ k: 10 });
function toBound(x) { return bound(x); }
function thrower(n) { if (n === 0) throw new RangeError('bottom ' + n); return thrower(n - 1); }
function Ctor(v) { if (v > 1) return wrap(v); this.v = v; }
function wrap(v) { return { wrapped: v }; }
let checksum = 0;
for (let round = 0; round < 400; round++) {
  checksum = (checksum + same(1500, round)) | 0;
  checksum = (checksum + shrink(1500, round, 0, 0)) | 0;
  checksum = (checksum + grow(1500)) | 0;
  checksum = (checksum + arrow(1500, round)) | 0;
  checksum = (checksum + even(1500 + (round & 1))) | 0;
  checksum = (checksum + toNative(round, 77) + toBound(round)) | 0;
  try { thrower(1000); } catch (e) { checksum = (checksum + e.message.length) | 0; }
  const made = new Ctor(round & 3);
  checksum = (checksum + (made.wrapped ?? made.v)) | 0;
}
console.log(checksum);
console.log(same(10, 0), shrink(10, 0, 0, 0), grow(10), arrow(10, 0), even(9), odd(9));
