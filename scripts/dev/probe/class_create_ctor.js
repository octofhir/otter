// Prototype-style classes: every constructor forwards to `initialize`.
var Class = { create: function () { return function () { this.initialize.apply(this, arguments); }; } };
var Vector = Class.create();
Vector.prototype = {
  initialize: function (x, y, z) { this.x = x; this.y = y; this.z = z; },
  subtract: function (w) { return new Vector(this.x - w.x, this.y - w.y, this.z - w.z); },
};
var Color = Class.create();
Color.prototype = { initialize: function (r, g) { this.r = r; this.g = g; } };
var a = new Vector(1, 2, 3), b = new Vector(0.5, 0.25, 0.125), s = 0;
for (var i = 0; i < 3000000; i++) { var c = a.subtract(b); s += c.x + c.y + c.z; if (i % 1000 === 0) new Color(i, i); }
console.log(s);
