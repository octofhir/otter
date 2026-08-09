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
console.log(JSON.stringify([
  result.value,
  result.targetMatches,
  Object.getPrototypeOf(result) === instancePrototype,
  prototypeGets
]));
