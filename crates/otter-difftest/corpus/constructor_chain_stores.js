// Constructor chains reached through `Base.call(this, ...)`: the base
// constructor stores into receivers of many subclass shapes, while each
// subclass allocation knows its receiver's shape. Every store must keep its
// meaning when the prototype chain, the receiver's extensibility, the
// constructor's prototype or the stored value kinds change after the hot
// paths were compiled.
"use strict";
var __extends = function (d, b) {
  for (var p in b) if (Object.prototype.hasOwnProperty.call(b, p)) d[p] = b[p];
  function __() { this.constructor = d; }
  __.prototype = b.prototype;
  d.prototype = new __();
};
var Span = function () {
  function Span() { this.minChar = -1; this.limChar = -1; }
  return Span;
}();
var Node = function (_super) {
  __extends(Node, _super);
  function Node(kind) {
    _super.call(this);
    this.kind = kind;
    this.flags = 0;
    this.typeLink = null;
    this.preComments = null;
    this.postComments = null;
  }
  return Node;
}(Span);
var kinds = [];
function subclass(i) {
  var C = function (_super) {
    __extends(C, _super);
    function C(a, b) {
      _super.call(this, i);
      this.a = a;
      this.b = b;
    }
    return C;
  }(Node);
  kinds.push(C);
  return C;
}
for (var i = 0; i < 12; i++) subclass(i);

function digest(o) {
  var keys = Object.keys(o);
  var s = keys.length + ":";
  for (var k = 0; k < keys.length; k++) {
    var v = o[keys[k]];
    s += keys[k] + "=" + (typeof v === "number" ? v : typeof v) + ";";
  }
  return s;
}
function build(n, seed) {
  var out = [];
  for (var j = 0; j < n; j++) {
    var C = kinds[(j + seed) % kinds.length];
    out.push(new C(j, j * 0.5));
  }
  return out;
}
function checksum(list) {
  var h = 0;
  for (var j = 0; j < list.length; j++) {
    var o = list[j];
    h = (h * 31 + o.kind + o.minChar + o.limChar + o.flags + o.a) | 0;
    h = (h + (o.b * 2)) | 0;
  }
  return h;
}

var results = [];
var keep = [];
for (var round = 0; round < 40; round++) {
  var list = build(2000, round);
  results.push(checksum(list));
  if (round % 5 === 0) keep.push(list); // old-generation growth between rounds
}
results.push(digest(build(1, 3)[0]));

// A setter on the base prototype now intercepts `this.flags = 0`.
var seen = 0;
Object.defineProperty(Span.prototype, "flags", {
  set: function (v) { seen += 1 + v; },
  get: function () { return 7; },
  configurable: true,
});
var withSetter = build(300, 1);
results.push(seen, checksum(withSetter), digest(withSetter[0]));
delete Span.prototype.flags;

// A read-only property on a prototype makes the strict-mode store throw,
// leaving the half-initialized receiver behind.
Object.defineProperty(Node.prototype, "typeLink", { value: "ro", writable: false, configurable: true });
var threw = 0;
var partial = null;
var Probe = kinds[4];
var probeProto = Probe.prototype;
for (var t = 0; t < 50; t++) {
  var obj = Object.create(probeProto);
  try { Probe.call(obj, t, t); } catch (e) { threw += e instanceof TypeError ? 1 : 100; partial = obj; }
}
results.push(threw, digest(partial));
delete Node.prototype.typeLink;
results.push(checksum(build(500, 7)));

// Receivers made non-extensible before the subclass stores.
var Sealing = function (_super) {
  __extends(Sealing, _super);
  function Sealing(mode) {
    _super.call(this, 99);
    if (mode === 1) Object.preventExtensions(this);
    if (mode === 2) Object.seal(this);
    if (mode === 3) Object.freeze(this);
    try { this.extra = mode; } catch (e) { this.caught = 1; }
    try { this.minChar = 5; } catch (e) { }
  }
  return Sealing;
}(Node);
var sealed = [];
for (var m = 0; m < 400; m++) {
  try { sealed.push(digest(new Sealing(m % 4))); } catch (e) { sealed.push("threw:" + (m % 4)); }
}
results.push(sealed.slice(0, 4).join("|"), sealed.slice(396).join("|"));

// Many properties: the receiver outgrows any initial in-object room.
function Wide(n) {
  for (var q = 0; q < n; q++) this["p" + q] = q;
  this.tail = n;
}
var wideSum = 0;
for (var w = 0; w < 600; w++) {
  var wide = new Wide(w % 40);
  wideSum += wide.tail + (wide.p3 || 0) + Object.keys(wide).length;
}
results.push(wideSum);

// The stored kinds change after compilation: int, double, object, string.
function Cell(v) { this.v = v; this.w = v; }
var cellSum = 0;
for (var c = 0; c < 3000; c++) {
  var value = c < 1000 ? c : c < 2000 ? c + 0.25 : c < 2500 ? { n: c } : "s" + c;
  var cell = new Cell(value);
  cellSum += typeof cell.v === "number" ? cell.w : typeof cell.v === "object" ? cell.w.n : cell.w.length;
}
results.push(cellSum);

// A constructor's prototype is replaced, and new.target picks another one.
function Point(x, y) { this.x = x; this.y = y; }
var pts = 0;
for (var r = 0; r < 2000; r++) {
  if (r === 1000) Point.prototype = { norm: function () { return this.x + this.y; } };
  var p = new Point(r, 1);
  pts += r >= 1000 ? p.norm() : p.x;
}
function Other() {}
Other.prototype.tag = "other";
var viaReflect = Reflect.construct(Point, [3, 4], Other);
results.push(pts, viaReflect.tag, viaReflect.x + viaReflect.y, Object.getPrototypeOf(viaReflect) === Other.prototype);

// A constructor that returns another object drops its receiver.
function Swap(x) { this.x = x; if (x % 2) return { x: -x }; }
var swapSum = 0;
for (var s = 0; s < 2000; s++) swapSum += new Swap(s).x;
results.push(swapSum);

for (var k = 0; k < keep.length; k++) results.push(checksum(keep[k]));
console.log(results.join(","));
