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
