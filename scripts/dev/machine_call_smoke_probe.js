// One hot body holding call sites of every kind, run long enough to reach the optimizing tier.
// Output must match Node exactly.
"use strict";

function add(a, b) { return a + b; }
function sum(...xs) { let s = 0; for (const x of xs) s += x; return s; }
function thrower(x) { if (x % 1000 === 999) throw new Error("boom" + x); return x; }
function self() { return this === undefined ? 0 : this.v; }
function forward() { return sum.apply(this, arguments); }
class Base { constructor(x) { this.x = x; } get() { return this.x; } }
class Derived extends Base { constructor(x) { super(x * 2); this.y = x; } get() { return super.get() + this.y; } }
class Spreader extends Base { constructor(...xs) { super(...xs); } }
function Plain(v) { this.v = v; }
function ReturnsObject(v) { return { v: v + 1 }; }
const bound = add.bind(null, 10);
const boundTwice = bound.bind(null, 5);
const proxy = new Proxy(add, {});
const trapped = new Proxy(add, { apply(t, self, args) { return t(...args) * 2; } });
const shapes = [
  { v: 1, m() { return this.v; } },
  { w: 0, v: 2, m() { return this.v * 2; } },
  { v: 3, m: function () { return this.v * 3; } },
  { u: 0, w: 0, v: 4, m() { return this.v * 4; } },
  { a: 0, u: 0, w: 0, v: 5, m() { return this.v * 5; } },
];

function hot(n) {
  let acc = 0;
  for (let i = 0; i < n; i++) {
    try { acc += thrower(i); } catch (e) { acc += e.message.length; }
  }
  return acc;
}

let total = 0;
for (let round = 0; round < 20; round++) total += hot(20000);
console.log(total);
