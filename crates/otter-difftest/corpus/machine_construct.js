let prototypeGets = 0;
const instancePrototype = { marker: "machine-construct" };

function Base(value) {
  this.value = value;
  this.targetMatches = new.target === Base;
  return 17;
}

Object.defineProperty(Base, "prototype", {
  configurable: true,
  get() {
    prototypeGets++;
    return instancePrototype;
  }
});

function construct(Ctor, value) {
  return new Ctor(value);
}

for (let i = 0; i < 5000; i++) construct(Base, i);
const result = construct(Base, 42);

let derivedBaseRuns = 0;
class DerivedBase {
  constructor(value) {
    derivedBaseRuns++;
    this.value = value;
  }
}
class Derived extends DerivedBase {
  constructor(value) {
    super(value);
  }
}
class DerivedReturner extends DerivedBase {
  constructor(value) {
    return value;
  }
}
function constructDerived(Ctor, value) {
  return new Ctor(value);
}
function constructDerivedReturner(Ctor, value) {
  return new Ctor(value);
}
for (let i = 0; i < 5000; i++) new Derived(i);
derivedBaseRuns = 0;
for (let i = 0; i < 5000; i++) {
  constructDerived(Derived, i);
  constructDerivedReturner(DerivedReturner, { warm: i });
}
const derived = constructDerived(Derived, 84);
const override = { marker: "derived-override" };
const returned = constructDerivedReturner(DerivedReturner, override);

function spreadAdd(left, right) {
  return left + right;
}
function callSpread(fn, values) {
  return fn(...values);
}
function SpreadBase(left, right) {
  this.total = left + right;
  this.targetMatches = new.target === SpreadBase;
}
function constructSpread(Ctor, values) {
  return new Ctor(...values);
}
class SpreadDerivedBase {
  constructor(left, right) {
    this.total = left + right;
  }
}
class SpreadDerived extends SpreadDerivedBase {
  constructor(values) {
    super(...values);
  }
}
function constructSpreadDerived(Ctor, values) {
  return new Ctor(values);
}
for (let i = 0; i < 5000; i++) {
  callSpread(spreadAdd, [i, 2]);
  constructSpread(SpreadBase, [i, 3]);
  constructSpreadDerived(SpreadDerived, [i, 4]);
}
const spreadCall = callSpread(spreadAdd, [40, 2]);
const spreadBase = constructSpread(SpreadBase, [39, 3]);
const spreadDerived = constructSpreadDerived(SpreadDerived, [38, 4]);

console.log(JSON.stringify([
  result.value,
  result.targetMatches,
  Object.getPrototypeOf(result) === instancePrototype,
  prototypeGets,
  derived.value,
  Object.getPrototypeOf(derived) === Derived.prototype,
  returned === override,
  derivedBaseRuns,
  spreadCall,
  spreadBase.total,
  spreadBase.targetMatches,
  spreadDerived.total,
  Object.getPrototypeOf(spreadDerived) === SpreadDerived.prototype
]));
