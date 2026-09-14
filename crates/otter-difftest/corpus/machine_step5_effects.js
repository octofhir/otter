// Step 5 widening: element proofs, calls, receiver allocation, constructors,
// conversions, and abrupt completion must agree in every execution tier.

function loadElement(receiver, index) {
  return receiver[index];
}

function storeElement(receiver, index, value) {
  receiver[index] = value;
  return receiver[index];
}

const dense = [10, 20, 30, 40];
const typedBuffer = new ArrayBuffer(16, { maxByteLength: 32 });
const typed = new Int32Array(typedBuffer);
typed.set([1, 2, 3, 4]);
for (let i = 0; i < 4100; i++) {
  loadElement(dense, i & 3);
  storeElement(typed, i & 3, i);
}

const holePrototype = { 1: "inherited" };
const holey = ["own", , "tail"];
Object.setPrototypeOf(holey, holePrototype);
dense["1.5"] = "fraction";
dense.NaN = "nan";
dense[4294967296] = "wide";

let loadThrow = "missing";
let storeThrow = "missing";
try {
  loadElement(null, 0);
} catch (error) {
  loadThrow = error.name;
}
try {
  storeElement(null, 0, 1);
} catch (error) {
  storeThrow = error.name;
}

const elementResults = [
  loadElement(dense, -0),
  loadElement(dense, 1.5),
  loadElement(dense, NaN),
  loadElement(dense, 4294967296),
  loadElement(holey, 1),
  loadElement(typed, 99),
  storeElement(typed, 1, 77),
  loadThrow,
  storeThrow,
];

let callEffects = 0;
function addOne(value) {
  callEffects++;
  return value + 1;
}
function addTwo(value) {
  callEffects++;
  return value + 2;
}
function invoke(target, value) {
  return target(value);
}
for (let i = 0; i < 4100; i++) invoke(addOne, i);
callEffects = 0;
const changedCall = invoke(addTwo, 40);

function sloppyThis() {
  return this === globalThis;
}
const bound = addOne.bind({ ignored: true });
const callResults = [changedCall, callEffects, invoke(bound, 40), sloppyThis()];

let conversions = 0;
const coerciveKey = {
  toString() {
    conversions++;
    return "2";
  },
};
const coerciveNumber = {
  valueOf() {
    conversions++;
    return 6;
  },
};
function convertedAccess(receiver, key) {
  return receiver[key];
}
function convertedAdd(value) {
  return value + 1;
}
for (let i = 0; i < 4100; i++) {
  convertedAccess(dense, 2);
  convertedAdd(i);
}
conversions = 0;
const conversionResults = [
  convertedAccess(dense, coerciveKey),
  convertedAdd(coerciveNumber),
  conversions,
];

let constructorEffects = 0;
function Base(value) {
  constructorEffects++;
  this.value = value;
}
function Override(value) {
  constructorEffects++;
  this.value = -1;
  return { value };
}
function construct(target, value) {
  return new target(value);
}
for (let i = 0; i < 4100; i++) construct(Base, i);
constructorEffects = 0;
const base = construct(Base, 41);
const override = construct(Override, 42);

class Parent {
  constructor(value) {
    constructorEffects++;
    this.value = value;
  }
}
class Derived extends Parent {
  constructor(value) {
    super(value);
    this.derived = true;
  }
}
for (let i = 0; i < 4100; i++) construct(Derived, i);
const derived = construct(Derived, 43);

let constructorThrow = "missing";
function Throwing() {
  constructorEffects++;
  throw new RangeError("ctor");
}
try {
  construct(Throwing, 0);
} catch (error) {
  constructorThrow = error.name;
}

const allocationRoots = [];
for (let i = 0; i < 256; i++) {
  allocationRoots.push({ index: i, payload: "step5-" + i });
}

console.log(JSON.stringify({
  elementResults,
  typed: Array.from(typed),
  callResults,
  conversionResults,
  constructors: [
    base.value,
    override.value,
    derived.value,
    derived.derived,
    constructorThrow,
    constructorEffects,
  ],
  allocation: [allocationRoots.length, allocationRoots[255].payload],
}));
