// Optimizing-tier property-access coverage under moving GC. Each object property
// load/store re-enters the VM through a reentrant transition stub that publishes a
// precise root map and reloads tagged roots after a possible relocation, so the hot
// property function must produce the interpreter-identical result at every GC stride.

globalThis.hotObject = { a: 1, b: 2, c: 0 };

// LoadProperty x3 (a, b, a), StoreProperty (c), int32 Add x2 — a whole-function
// eligible read-modify-write over named properties.
function hotProperty(o) {
  o.c = o.a + o.b;
  return o.c + o.a;
}

// A float property chain to exercise Float64 values crossing a property transition.
function hotFloatProperty(o) {
  o.total = o.x * o.y + o.z;
  return o.total;
}

let warm = "";
for (let i = 0; i < 4010; i++) {
  warm += "hotProperty(hotObject);";
  warm += "hotFloatProperty({ x: 1.5, y: 2.0, z: 0.25, total: 0 });";
}
eval(warm);

// Drive the optimized functions over freshly allocated objects so a collection can
// fire while a property transition is in flight (the receiver is a tagged root that
// may relocate mid-store).
let propSum = 0;
let floatSum = 0;
for (let k = 0; k < 800; k++) {
  const o = { a: k & 7, b: (k + 1) & 7, c: 0 };
  propSum += hotProperty(o);
  const f = { x: (k & 3) + 0.5, y: 2.0, z: 0.25, total: 0 };
  floatSum += hotFloatProperty(f);
}
globalThis.propSum = propSum;
globalThis.propFloatSum = floatSum;

let propThrow = "";
try { hotProperty(null); } catch (error) { propThrow = error.name; }
globalThis.propThrow = propThrow;

// A wholly-eligible method that reads `this` — LoadThis x3 + property RMW, no loop
// counter/global/call — so it runs through the optimizing tier and exercises the
// tagged `this` load under GC stress.
function Point(a, b) { this.a = a; this.b = b; this.c = 0; }
Point.prototype.mix = function () {
  this.c = this.a + this.b;
  return this.c + this.a;
};
let warmThis = "";
for (let i = 0; i < 4010; i++) { warmThis += "sink.mix();"; }
globalThis.sink = new Point(1, 2);
eval(warmThis);
let thisSum = 0;
for (let k = 0; k < 800; k++) {
  const p = new Point(k & 7, (k + 1) & 7);
  thisSum += p.mix();
}
globalThis.thisSum = thisSum;

function constructWith(C, x, y) {
  return new C(x, y);
}
let warmConstruct = "";
for (let i = 0; i < 4010; i++) {
  warmConstruct += "constructWith(Point, 1, 2);";
}
eval(warmConstruct);
let constructSum = 0;
for (let i = 0; i < 800; i++) {
  const p = constructWith(Point, i & 7, (i + 1) & 7);
  constructSum += p.a + p.b;
}

// Regression distilled from RayTrace's Vector.initialize: float parameters
// flow through truthy branches into tagged phis and then StoreProperty. The
// method must stay optimized across all three stores under every GC stride.
function TruthyVector() {}
TruthyVector.prototype.reset = function (x, y, z) {
  this.x = x ? x : 0;
  this.y = y ? y : 0;
  this.z = z ? z : 0;
  return 0;
};

globalThis.truthyVector = new TruthyVector();
let warmTruthyVector = "";
for (let i = 0; i < 4010; i++) {
  warmTruthyVector += "new TruthyVector().reset(1.5, 2.5, 3.5);";
}
eval(warmTruthyVector);

let truthyVectorSum = 0;
for (let i = 0; i < 800; i++) {
  truthyVector.reset(
    (i & 1) ? 1.25 : 0,
    (i & 2) ? 2.5 : 0,
    (i & 4) ? 3.75 : 0
  );
  truthyVectorSum += truthyVector.x + truthyVector.y + truthyVector.z;
}

console.log(JSON.stringify({
  propSum,
  floatSum,
  propThrow,
  thisSum,
  constructSum,
  truthyVectorSum
}));
