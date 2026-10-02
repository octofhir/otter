// Hot call sites of every kind, run long enough to reach the optimizing tier.
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

const cases = [
  (i) => { let acc = 0; acc += add(i, 1); return acc; },
  (i) => { let acc = 0; acc += sum(i, 2, 3); return acc; },
  (i) => { let acc = 0; acc += self.call({ v: i & 7 }); return acc; },
  (i) => { let acc = 0; acc += forward.call({}, 1, 2, i & 3); return acc; },
  (i) => { let acc = 0; acc += new Base(i).get(); return acc; },
  (i) => { let acc = 0; acc += new Derived(i & 15).get(); return acc; },
  (i) => { let acc = 0; acc += new Spreader(...[i & 3]).get(); return acc; },
  (i) => { let acc = 0; acc += new Plain(i).v; return acc; },
  (i) => { let acc = 0; acc += new ReturnsObject(i).v; return acc; },
  (i) => { let acc = 0; acc += bound(i & 1); return acc; },
  (i) => { let acc = 0; acc += boundTwice(); return acc; },
  (i) => { let acc = 0; acc += proxy(1, i & 1); return acc; },
  (i) => { let acc = 0; acc += trapped(1, 1); return acc; },
  (i) => { let acc = 0; acc += shapes[i % 5].m(); return acc; },
  (i) => { let acc = 0; acc += Math.max(i & 7, 3); return acc; },
  (i) => { let acc = 0; acc += sum(...[1, i & 1, 2]); return acc; },
  (i) => { let acc = 0; acc += self.call(...[{ v: 2 }]); return acc; },
  (i) => { let acc = 0; try { acc += thrower(i); } catch (e) { acc += e.message.length; } return acc; },
];

function hot(n, selected) {
  let acc = 0;
  for (let i = 0; i < n; i++) {
    for (const index of selected) acc += cases[index](i);
  }
  return acc;
}

const only = process.argv.slice(2).map(Number).filter((x) => !Number.isNaN(x));
const selected = only.length ? only : cases.map((_, index) => index);
let total = 0;
for (let round = 0; round < 20; round++) total += hot(20000, selected);
console.log(total);
