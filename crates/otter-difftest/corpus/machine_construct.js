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

console.log(JSON.stringify([
  result.value,
  result.targetMatches,
  Object.getPrototypeOf(result) === instancePrototype,
  prototypeGets,
  derived.value,
  Object.getPrototypeOf(derived) === Derived.prototype,
  returned === override,
  derivedBaseRuns
]));
