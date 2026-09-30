// A function's `prototype` lives in its own slot: reads, assignments,
// redefinitions and freezing must agree with construct, instanceof and the
// own-property operations in every tier.
"use strict";

function keys(f) {
  return Reflect.ownKeys(f)
    .filter((key) => key !== "arguments" && key !== "caller")
    .map(String)
    .join();
}

function Fresh() {}
console.log("prototype" in Fresh, keys(Fresh));
Fresh.x = 1;
console.log(keys(Fresh), Fresh.hasOwnProperty("prototype"));

function Defined() {}
Object.defineProperty(Defined, "prototype", { value: { a: 1 } });
console.log(JSON.stringify(Object.getOwnPropertyDescriptor(Defined, "prototype")));
Object.defineProperty(Defined, "prototype", { writable: false });
const definedProto = Defined.prototype;
try {
  Defined.prototype = {};
} catch (e) {
  console.log(e instanceof TypeError, Defined.prototype === definedProto);
}
try {
  delete Defined.prototype;
} catch (e) {
  console.log("delete", e instanceof TypeError);
}
try {
  Object.defineProperty(Defined, "prototype", { enumerable: true });
} catch (e) {
  console.log("redefine", e instanceof TypeError);
}

// Assignment before and after the default object is allocated, observed by
// construct and instanceof once the callers are hot.
function Base(v) {
  this.v = v;
}
Base.prototype.get = function () {
  return this.v;
};
function Swapped(v) {
  this.v = v;
}
const first = Swapped.prototype;
Swapped.prototype = { get() { return -this.v; } };
function make(C, i) {
  return new C(i);
}
function test(o, C) {
  return o instanceof C;
}
let sum = 0;
let hits = 0;
for (let i = 0; i < 30000; i++) {
  const C = i & 1 ? Base : Swapped;
  const o = make(C, i);
  sum += o.get();
  if (test(o, C)) hits++;
  if (i === 20000) Swapped.prototype = first;
  if (i === 20001) first.get = function () { return 2 * this.v; };
}
console.log(sum, hits, test(make(Swapped, 3), Swapped), make(Swapped, 3).get());

// Closures created in a loop each own their slot.
const made = [];
for (let i = 0; i < 2000; i++) {
  const F = function (x) {
    this.x = x;
  };
  if (i % 3 === 0) F.prototype = { tag: i };
  if (i % 5 === 0) Object.defineProperty(F, "prototype", { writable: false });
  made.push(F);
}
let tags = 0;
let own = 0;
for (let i = 0; i < made.length; i++) {
  const o = new made[i](i);
  if (o.tag === i) tags++;
  if (Object.getPrototypeOf(o) === made[i].prototype && o instanceof made[i]) own++;
}
console.log(tags, own, Object.getOwnPropertyDescriptor(made[5], "prototype").writable);

// Kinds without an implicit prototype keep a user-made one as ordinary.
const arrow = () => 1;
const method = { m() {} }.m;
console.log("prototype" in arrow, "prototype" in method);
arrow.prototype = 3;
console.log(arrow.prototype, keys(arrow), delete arrow.prototype, "prototype" in arrow);

// Generator functions: prototype inherits the shared generator prototype and
// has no constructor.
function* gen() {
  yield 1;
}
const GeneratorPrototype = Object.getPrototypeOf(function* () {}).prototype;
console.log(
  Object.getPrototypeOf(gen.prototype) === GeneratorPrototype,
  Object.getOwnPropertyNames(gen.prototype).length,
  Object.getPrototypeOf(gen()) === gen.prototype,
);

// Freezing the function freezes the slot.
function Frozen() {}
Object.freeze(Frozen);
console.log(Object.getOwnPropertyDescriptor(Frozen, "prototype").writable, Object.isFrozen(Frozen));

// Classes: non-writable from creation.
class K {}
console.log(JSON.stringify(Object.getOwnPropertyDescriptor(K, "prototype").writable), keys(K));
console.log("prototype" in Fresh.bind(null), Fresh["prototype"] === Fresh.prototype);
