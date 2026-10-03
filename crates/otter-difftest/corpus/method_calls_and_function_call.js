// Method calls and Function.prototype.call in optimized code: f.call with
// zero, one and many arguments, an f.call whose `call` is replaced, guarded
// methods on own slots and prototypes, a prototype method replaced mid-run,
// sloppy callees receiving primitive and nullish receivers, and constructors
// chained through `_super.call(this, ...)`.
function Base(a) { this.a = a; }
function Derived(a, b) { Base.call(this, a); this.b = b; }
Derived.prototype.sum = function () { return this.a + this.b; };
function sloppyThis() { return typeof this; }
const holder = { k: 3, own() { return this.k; } };
function viaCall(f, t, x, y) { return f.call(t, x, y); }
function noArgs(f) { return f.call(); }
function method(o) { return o.sum(); }
function ownMethod(o) { return o.own(); }
let out = [];
for (let r = 0; r < 3000; r++) {
  const d = new Derived(r, 1);
  out = [method(d), ownMethod(holder), viaCall(sloppyThis, 5), viaCall(sloppyThis, null), noArgs(sloppyThis),
         viaCall(function (x, y) { 'use strict'; return [this, x, y].join(); }, 'T', 1, 2)];
  if (r === 1500) Derived.prototype.sum = function () { return -1; };
  if (r === 2000) holder.own = function () { return 'replaced'; };
  if (r === 2500) sloppyThis.call = function () { return 'own call'; };
}
console.log(out.join(' | '));
