// `callee.apply(this, arguments)` forwarding: the intrinsic path never
// materializes an arguments object; an overridden `apply` receives the
// activation's mapped object, the same one across both forwards.
var Class = { create: function() { return function() { this.initialize.apply(this, arguments); }; } };
var Point = Class.create();
Point.prototype.initialize = function(x, y, z) { this.x = x; this.y = y; this.z = z === undefined ? 0 : z; this.n = arguments.length; };
function twice(a, b) {
  seen.length = 0;
  first.apply(this, arguments);
  second.apply(this, arguments);
  return a + "," + b + "," + (seen[0] === seen[1]);
}
var seen = [];
function first() { return 1; }
function second() { return 2; }
first.apply = function(receiver, list) { seen.push(list); list[0] = "A"; return 0; };
second.apply = function(receiver, list) { seen.push(list); return 0; };
function strictForward(a, b) { "use strict"; return target.apply(null, arguments); }
function target() { return arguments.length + ":" + arguments[0] + ":" + (this === globalThis); }
function arrowRef(a) { const f = () => target.apply(null, arguments); return f(); }
let acc = 0;
let tail = "";
for (let i = 0; i < 3000; i++) {
  const p = new Point(i, i + 1);
  acc += p.x + p.y + p.z + p.n;
  tail = twice(i, 1) + "|" + strictForward(i, 2, 3) + "|" + arrowRef(i);
}
console.log(acc + "|" + tail);
