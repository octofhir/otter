// Numeric field history must preserve Number semantics, one committed store,
// descriptor/prototype semantics and moving children across native tier-up.
"use strict";
function check(ok, message) { if (!ok) throw new Error(message); }
function equal(actual, expected, message) {
  check(Object.is(actual, expected), message + ": " + String(actual));
}
function literal() { return {literalMarker: 1, x: 0, y: 0, pending: undefined}; }
function Vector(flag) {
  this.x = 0;
  if (flag) this.y = 0; else this.y = 0;
  this.pending = undefined;
}
function Simple(x) { this.x = x; this.pending = undefined; }
function update(o, x, y) { o.x = x; o.y = y; return o.x + o.y; }
let warmLiteral, warmVector;
for (let i = 0; i < 6000; i++) {
  warmLiteral = literal();
  warmVector = new Vector(i & 1);
}
for (let i = 0; i < 20000; i++) update(warmLiteral, i & 7, i & 3);
update(warmLiteral, 1.25, 2.5);
update(warmVector, 3.5, 4.25);
const simple = new Simple(1.5);
equal(simple.pending, undefined, "simple constructor undefined");
for (let i = 0; i < 128; i++) {
  const a = literal(), b = new Vector(i & 1), c = new Simple(0);
  equal(a.x, 0, "literal zero after promotion");
  equal(a.y, 0, "literal second zero after promotion");
  equal(b.x, 0, "branched constructor zero after promotion");
  equal(b.y, 0, "branched constructor second zero after promotion");
  equal(c.x, 0, "simple constructor zero after promotion");
  equal(a.pending, undefined, "literal pending undefined");
  equal(b.pending, undefined, "constructor pending undefined");
}

let beforeAssignment = 0;
function inspectReceiver(o) {
  equal(o.later, undefined, "constructor read before assignment");
  check(!Object.prototype.hasOwnProperty.call(o, "later"), "constructor field appeared early");
  beforeAssignment++;
}
function Observable(x) { this.first = 0; inspectReceiver(this); this.later = x; }
for (let i = 0; i < 256; i++) equal(new Observable(i ? 0 : 1.5).later, i ? 0 : 1.5, "observable constructor");
equal(beforeAssignment, 256, "constructor callback count");

const special = {specialMarker: 0, x: 0};
const numbers = [0, -0, 1.25, NaN, Infinity, -Infinity, 2147483648, -2147483649, 7];
const labels = [];
for (const number of numbers) {
  special.x = number;
  equal(special.x, number, "special number roundtrip");
  labels.push(number !== number ? "NaN" : Object.is(number, -0) ? "-0" : String(number));
}
special.x = undefined;
equal(special.x, undefined, "undefined in promoted field");
special.x = 17;
equal(special.x, 17, "integer after undefined");

let getterCalls = 0, argumentCalls = 0, setterCalls = 0, keyCalls = 0;
let supplied = 1.5;
const source = {get value() { getterCalls++; return supplied; }};
function argument(value) { argumentCalls++; return value; }
function assign(target) { target.x = source.value + argument(1); return target.x; }
const direct = {directMarker: 0, x: 0};
for (let i = 0; i < 20000; i++) assign(direct);
getterCalls = 0;
argumentCalls = 0;
supplied = "general";
equal(assign(direct), "general1", "generalization after evaluated RHS");
equal(getterCalls, 1, "getter must not replay on a refused numeric store");
equal(argumentCalls, 1, "argument must not replay on a refused numeric store");
const directCounts = [getterCalls, argumentCalls];

const setterPrototype = {set target(value) { setterCalls++; this.saved = value; }};
const throughSetter = Object.create(setterPrototype);
function assignSetter(o) { o.target = source.value + argument(2); }
getterCalls = 0;
argumentCalls = 0;
assignSetter(throughSetter);
equal(throughSetter.saved, "general2", "setter value");
equal(setterCalls, 1, "setter called once");
equal(getterCalls, 1, "setter RHS getter called once");
equal(argumentCalls, 1, "setter RHS argument called once");
function computedKey() { keyCalls++; return "x"; }
function assignKey(o, value) { o[computedKey()] = argument(value); }
const keyed = {keyedMarker: 0, x: 0};
assignKey(keyed, 1.25);
keyCalls = 0;
argumentCalls = 0;
assignKey(keyed, null);
equal(keyed.x, null, "keyed generalization");
equal(keyCalls, 1, "computed key called once");
equal(argumentCalls, 1, "keyed argument called once");

const first = {polyA: 0, x: 0, y: 0}, second = {polyB: 0, y: 0, x: 0};
for (let i = 0; i < 20000; i++) update(i & 1 ? first : second, i & 7, i & 3);
equal(update(first, 1.25, 2.5), 3.75, "first polymorphic layout");
equal(update(second, 3.5, 4.25), 7.75, "second polymorphic layout");
first.x = "wide";
equal(first.x, "wide", "general tagged value");
equal(first.y, 2.5, "neighbor numeric field survives generalization");
second.y = undefined;
equal(second.x, 3.5, "other property order numeric field");
equal(second.y, undefined, "other property order undefined");

const changed = {stateMarker: 0, x: 1.25, y: 2.5};
Object.setPrototypeOf(changed, {inherited: 31});
Object.defineProperty(changed, "x", {enumerable: false});
Object.preventExtensions(changed);
equal(changed.x, 1.25, "prototype/descriptor/state replay x");
equal(changed.y, 2.5, "prototype/descriptor/state replay y");
equal(changed.inherited, 31, "new prototype");
equal(Object.keys(changed).join(","), "stateMarker,y", "descriptor key order");
Object.freeze(changed);
let frozenThrows = 0;
try { changed.y = 4.5; } catch (error) { check(error instanceof TypeError, "freeze error"); frozenThrows++; }
equal(frozenThrows, 1, "frozen numeric write throws");
equal(changed.y, 2.5, "frozen numeric word unchanged");

// Four literal slots fix the initial inline footprint. Named appends retain
// fast shapes past the flat-key-directory threshold and into the suffix slab.
function overflowSeed() { return {overflowMarker: 0, a: 0, b: 0, c: 0}; }
function extendNamed(o) {
  o.k00 = 0; o.k01 = 0; o.k02 = 0; o.k03 = 0; o.k04 = 0;
  o.k05 = 0; o.k06 = 0; o.k07 = 0; o.k08 = 0; o.k09 = 0;
  o.k10 = 0; o.k11 = 0; o.k12 = 0; o.k13 = 0; o.k14 = 0;
  o.k15 = 0; o.k16 = 0; o.k17 = 0; o.k18 = 0; o.k19 = 0;
}
function updateOverflow(o, value) { o.k19 = value; return o.k19; }
const overflow = overflowSeed();
extendNamed(overflow);
for (let i = 0; i < 20000; i++) updateOverflow(overflow, i & 7);
equal(updateOverflow(overflow, 3.5), 3.5, "flat-directory overflow double store/load");
overflow.k00 = 1.25;
equal(overflow.k19, 3.5, "suffix number survives earlier-field change");
equal(updateOverflow(overflow, 7), 7, "suffix integer and index masking");
equal(updateOverflow(overflow, undefined), undefined, "suffix undefined");
equal(updateOverflow(overflow, 9.5), 9.5, "suffix number after undefined");
Object.preventExtensions(overflow);
equal(overflow.k00, 1.25, "flat-directory state replay first suffix field");
equal(overflow.k19, 9.5, "flat-directory state replay last suffix field");
equal(Object.keys(overflow).length, 24, "flat-directory semantic key count");
const overflowPeer = overflowSeed();
extendNamed(overflowPeer);
equal(overflowPeer.k00, 0, "earlier suffix zero birth after promotion");
equal(overflowPeer.k19, 0, "last suffix zero birth after promotion");

const captured = {captureMarker: 0, x: 1.25, y: 9.5};
function capture(owner) { return function read() { return owner.x; }; }
const readCaptured = capture(captured);
const child = {marker: 731, nested: {marker: 719}};
captured.x = child;
if (typeof gc === "function") gc();
check(readCaptured() === child, "captured child identity after collection");
equal(readCaptured().nested.marker, 719, "captured moving graph");
equal(captured.y, 9.5, "neighbor double after moving child");
warmLiteral.x = child;
const freshGeneral = literal();
equal(freshGeneral.x, 0, "zero birth after absorbing general history");
equal(freshGeneral.y, 0, "double sibling birth after general history");
equal(Object.keys(freshGeneral).join(","), "literalMarker,x,y,pending", "literal own key order");

// Native numeric words preserve Number semantics as keys, descriptors,
// dictionary properties and global-object bindings. Int32 zero and Double zero
// denote the same ordinary JS value at all of these boundaries.
const nativeZero = new Vector(1);
const numericKeys = new Map([[0, "zero"], [7, "seven"]]);
equal(numericKeys.get(nativeZero.x), "zero", "native zero Map key");
equal(new Set([0, nativeZero.x]).size, 1, "native zero Set key");
Object.defineProperty(nativeZero, "x", {enumerable: false});
equal(Object.getOwnPropertyDescriptor(nativeZero, "x").value, 0, "descriptor canonical zero");
Object.setPrototypeOf(nativeZero, {inheritedNumericMarker: 719});
equal(nativeZero.x, 0, "state transition canonical zero");
equal(numericKeys.get(nativeZero.x), "zero", "state transition Map key");

globalThis.numericWordGlobal = 0;
function writeNumericGlobal(value) { globalThis.numericWordGlobal = value * 1; }
function readNumericGlobal() { return numericWordGlobal; }
for (let i = 0; i < 12000; i++) {
  writeNumericGlobal(i & 7);
  equal(readNumericGlobal(), i & 7, "native global numeric read");
}
for (const value of [0, -0, 1.25, NaN, Infinity, -Infinity, 7]) {
  writeNumericGlobal(value);
  equal(readNumericGlobal(), value, "global object numeric word");
  equal(Object.getOwnPropertyDescriptor(globalThis, "numericWordGlobal").value,
    value, "global object numeric descriptor");
}
equal(numericKeys.get(readNumericGlobal()), "seven", "global numeric Map key");

// The computed key is produced by F64 arithmetic. Native cached element
// access must accept integral Double words without changing the original key
// that a fractional, named, descriptor or prototype miss completes.
function numericIndexedWrite(array, index, value) {
  const key = index * 1.25;
  array[key] = value;
  return array[key];
}
const numericIndexed = [];
for (let i = 0; i < 6000; i++) {
  equal(numericIndexedWrite(numericIndexed, (i & 7) * 4, i), i, "Double indexed read/write");
}
const indexedLength = numericIndexed.length;
for (const index of [-0, -0.8, 0.4, NaN, Infinity, 3435973836]) {
  equal(numericIndexedWrite(numericIndexed, index, 731), 731, "numeric indexed hit or committed miss");
}
equal(numericIndexed.length, indexedLength, "non-index Double keys retain array length");
equal(numericIndexed["0.5"], 731, "fractional original key");
equal(numericIndexed["NaN"], 731, "NaN original key");
equal(numericIndexed["4294967295"], 731, "array-index upper boundary is a named key");
let indexedSetterCalls = 0, indexedSaved;
const numericIndexedPrototype = {};
Object.defineProperty(numericIndexedPrototype, "5", {
  get() { return indexedSaved; },
  set(value) { indexedSetterCalls++; indexedSaved = value; }
});
const numericIndexedInherited = [];
Object.setPrototypeOf(numericIndexedInherited, numericIndexedPrototype);
equal(numericIndexedWrite(numericIndexedInherited, 4, 719), 719, "Double key prototype setter");
equal(indexedSetterCalls, 1, "Double key setter commits once");
check(!Object.prototype.hasOwnProperty.call(numericIndexedInherited, "5"), "setter creates no own element");

console.log(JSON.stringify({
  numbers: labels,
  constructors: beforeAssignment,
  direct: directCounts,
  setter: setterCalls,
  keyed: [keyCalls, argumentCalls],
  polymorphic: [first.x, first.y, second.x, second.y === undefined],
  frozen: frozenThrows,
  captured: [readCaptured().marker, readCaptured().nested.marker, captured.y],
  fresh: [freshGeneral.x, freshGeneral.y, freshGeneral.pending === undefined],
  indices: [indexedLength, numericIndexed.length, indexedSetterCalls, indexedSaved]
}));
