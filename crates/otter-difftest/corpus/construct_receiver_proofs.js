// Constructors whose optimized entry proves `this` has its family's root
// shape and whose megamorphic stores run the proved receiver's program: a
// base constructor shared by many subclasses, entries with another
// new.target (Reflect.construct, a derived class's super), a plain call,
// an accessor or read-only property appearing on the prototype chain while
// the code is hot, and a replaced prototype.
'use strict';
const log = [];
function Base(kind) {
  this.kind = kind;
  this.flags = 0;
  this.start = -1;
  this.end = -1;
  this.parent = null;
}
const classes = [];
for (let i = 0; i < 24; i++) {
  const C = new Function('Base', `return function C${i}(k) { Base.call(this, k); this.extra${i} = ${i}; }`)(Base);
  C.prototype = Object.create(Base.prototype);
  classes.push(C);
}
let sum = 0;
for (let r = 0; r < 3000; r++) {
  for (let i = 0; i < classes.length; i++) {
    const o = new classes[i](i);
    sum += o.kind + o.flags + o.start + o.end + (o.parent === null ? 1 : 0) + o['extra' + i];
  }
}
log.push(sum);

// A constructor proved hot, then entered with other receivers.
function Point(x, y) {
  this.x = x;
  this.y = y;
  this.len = x + y;
}
let points = 0;
for (let i = 0; i < 20000; i++) {
  const p = new Point(i, i + 1);
  points += p.len;
}
class Sub extends Point {
  constructor(x) {
    super(x, 2);
    this.z = 3;
  }
}
for (let i = 0; i < 2000; i++) {
  const s = new Sub(i);
  points += s.len + s.z;
  const r = Reflect.construct(Point, [i, 1], Sub);
  points += r.len + (r instanceof Sub ? 1 : 0);
  const bag = {};
  Point.call(bag, i, 4);
  points += bag.len;
}
for (let i = 0; i < 20000; i++) points += new Point(i, 2).len;
log.push(points);

// An accessor appears on the prototype for one stored name: later stores
// run it instead of adding an own property.
function Rec(v) {
  this.a = v;
  this.b = v + 1;
}
let recs = 0;
let setterRuns = 0;
for (let i = 0; i < 30000; i++) {
  if (i === 20000) {
    Object.defineProperty(Rec.prototype, 'b', {
      set(v) { setterRuns++; },
      get() { return -1; },
      configurable: true,
    });
  }
  const r = new Rec(i);
  recs += r.a + r.b + (Object.prototype.hasOwnProperty.call(r, 'b') ? 1 : 0);
}
log.push(recs, setterRuns);

// A read-only inherited property makes the strict store throw.
function Ro(v) {
  this.p = v;
  this.q = v;
}
let roThrows = 0;
let ro = 0;
for (let i = 0; i < 30000; i++) {
  if (i === 25000) Object.defineProperty(Ro.prototype, 'q', { value: 7, writable: false, configurable: true });
  try {
    ro += new Ro(i).q;
  } catch (e) {
    roThrows += e instanceof TypeError ? 1 : 100;
  }
}
log.push(ro, roThrows);

// The prototype replaced while hot gives receivers a new root shape.
function Swap(v) {
  this.v = v;
  this.w = v * 2;
}
let swaps = 0;
for (let i = 0; i < 30000; i++) {
  if (i === 15000) Swap.prototype = { tag: 5 };
  const s = new Swap(i);
  swaps += s.v + s.w + (s.tag === 5 ? 1 : 0);
}
log.push(swaps);
console.log(JSON.stringify(log));
