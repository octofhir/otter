// Constructors that forward their arguments to an inherited initializer,
// spliced at hot `new` sites: the initializer each class's receiver inherits,
// a replaced initializer, mapped formals, extra and missing arguments, and
// receivers whose prototype chain changes.
const results = [];
var Class = { create: function () { return function () { this.initialize.apply(this, arguments); }; } };

var Point = Class.create();
Point.prototype = {
  initialize: function (x, y) { this.x = x; this.y = y; },
  add: function (o) { return new Point(this.x + o.x, this.y + o.y); },
  norm: function () { return this.x * this.x + this.y * this.y; },
};
var Tagged = Class.create();
Tagged.prototype = {
  initialize: function (tag, value) { this.tag = tag; this.value = value === undefined ? 'none' : value; },
};
var Counter = Class.create();
Counter.prototype = { initialize: function () { this.count = arguments.length; } };

let total = 0;
let p = new Point(0, 0);
const step = new Point(1, 2);
for (let i = 0; i < 20000; i++) {
  p = p.add(step);
  if (i % 7 === 0) total += new Tagged('t' + (i & 3), i).value;
  if (i % 11 === 0) total += new Counter(i, i, i).count;
}
results.push(p.x, p.y, p.norm(), total, new Tagged('only').value);

// The initializer the prototype holds changes after the site is hot.
function makePoints(n) {
  let s = 0;
  for (let i = 0; i < n; i++) s += new Point(i, 1).norm();
  return s;
}
results.push(makePoints(5000));
Point.prototype.initialize = function (x, y) { this.x = y; this.y = x; this.swapped = true; };
results.push(makePoints(5000), new Point(3, 4).swapped);
delete Point.prototype.initialize;
Object.getPrototypeOf(Point.prototype).initialize = function (x) { this.x = this.y = x; };
results.push(makePoints(100));
delete Object.prototype.initialize;
Point.prototype.initialize = function (x, y) { this.x = x; this.y = y; };

// A receiver that owns the key before construction runs.
var Own = function () { this.initialize = function (v) { this.v = v * 2; }; this.initialize.apply(this, arguments); };
Own.prototype.initialize = function (v) { this.v = v; };
let owned = 0;
for (let i = 0; i < 20000; i++) owned += new Own(i).v;
results.push(owned);

// Mapped formals: the forwarded list carries a formal's current value.
var Mapped = function (a, b) { a = a * 10; this.initialize.apply(this, arguments); };
Mapped.prototype.initialize = function (a, b, c) { this.sum = a + (b === undefined ? 0 : b) + (c === undefined ? 0 : c); };
let mapped = 0;
for (let i = 0; i < 20000; i++) mapped += new Mapped(i, 1, 2).sum + new Mapped(i).sum;
results.push(mapped);

// A strict constructor forwards an unmapped list.
var Strict = function (a) { 'use strict'; a = 0; this.initialize.apply(this, arguments); };
Strict.prototype.initialize = function (a) { this.a = a; };
let strict = 0;
for (let i = 0; i < 20000; i++) strict += new Strict(i).a;
results.push(strict);

// The forwarded callee throws: the exception reaches the `new` site.
var Throws = Class.create();
Throws.prototype = { initialize: function (n) { if (n === 15000) throw new RangeError('n ' + n); this.n = n; } };
let caught = 0;
for (let i = 0; i < 20000; i++) {
  try { caught += new Throws(i).n & 1; } catch (e) { results.push(e.message); }
}
results.push(caught);

console.log(JSON.stringify(results));
