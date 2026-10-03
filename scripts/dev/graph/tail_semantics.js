"use strict";
// Tail-call edge cases whose observable results must not depend on whether
// the caller's frame is replaced: callee kinds, throws, constructs, receivers.
const log = [];
function toMax(a, b) { return Math.max(a, b); }
function toBound(x) { return bound(x); }
const bound = function (x) { return [this === undefined ? 'u' : typeof this, x]; }.bind({ k: 1 });
class K { constructor(v) { this.v = v; } }
function toClass(v) { return K(v); }
function toNonCallable(v) { return v(); }
function thrower(n) { if (n === 0) throw new Error('deep'); return thrower(n - 1); }
function asCtor(x) { if (new.target) return helper(x); return helper(x + 1); }
function helper(x) { return { x }; }
function selfArgs(a, b, c, d, e) { return a === 0 ? [b, c, d, e] : selfArgs(a - 1, b, c); }
function receiverCheck() { return thisOf(); }
function thisOf() { return this; }
function manyArgs(n, a, b, c, d, e, f, g, h, i) { return n === 0 ? a + b + c + d + e + f + g + h + i : fewArgs(n - 1, a); }
function fewArgs(n, a) { return manyArgs(n, a, 1, 2, 3, 4, 5, 6, 7, 8); }
function getterTail(o) { return o.f(o.v); }
const gt = { get f() { log.push('get'); return (v) => v * 2; }, v: 21 };
for (let round = 0; round < 3000; round++) {
  toMax(round, 7); toBound(round); selfArgs(3, 1, 2, 3, 4); receiverCheck(); manyArgs(4, 1);
  try { toClass(1); } catch (e) {}
  try { toNonCallable(5); } catch (e) {}
  try { thrower(20); } catch (e) {}
  new asCtor(1); asCtor(2); getterTail(gt);
}
log.length = 0;
console.log(toMax(3, 9), JSON.stringify(toBound(4)));
try { toClass(1); } catch (e) { console.log(e.constructor.name, e.message.includes('K') || e.message.length > 0); }
try { toNonCallable(5); } catch (e) { console.log(e.constructor.name); }
try { thrower(50000); } catch (e) { console.log(e.message); }
const made = new asCtor(1);
console.log(made instanceof asCtor, JSON.stringify(made), JSON.stringify(asCtor(2)));
console.log(JSON.stringify(selfArgs(5, 1, 2, 3, 4)));
console.log(receiverCheck());
console.log(manyArgs(200000, 1));
console.log(getterTail(gt), log.join());
