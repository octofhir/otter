// A method inlined from a prototype slot runs only while that slot still
// holds it: reassigning the method, redefining it as an accessor, shadowing
// it on an intermediate prototype, or swapping a prototype must each take
// effect on the very next call from hot code.
function Base(v) { this.v = v; }
Base.prototype.get = function () { return this.v; };
function Derived(v) { Base.call(this, v); }
Derived.prototype = Object.create(Base.prototype);
Derived.prototype.twice = function () { return this.get() * 2; };

function run(objects, n, hook) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    if (hook) hook(i);
    const o = objects[i & 3];
    sum += o.twice() + o.get();
  }
  return sum;
}

const objects = [new Derived(1), new Derived(2), new Derived(3), new Derived(4)];
const results = [];
results.push(run(objects, 20000, null));
results.push(run(objects, 20000, (i) => {
  if (i === 10000) Base.prototype.get = function () { return this.v + 100; };
}));
results.push(run(objects, 20000, (i) => {
  if (i === 5000) Derived.prototype.get = function () { return -this.v; };
}));
results.push(run(objects, 20000, (i) => {
  if (i === 7000) {
    Object.defineProperty(Derived.prototype, "get", {
      get() { return () => 7; },
      configurable: true,
    });
  }
}));
results.push(run(objects, 20000, (i) => {
  if (i === 9000) {
    delete Derived.prototype.get;
    Object.setPrototypeOf(Derived.prototype, { get() { return 1000; } });
  }
}));
results.push(run(objects, 20000, (i) => {
  if (i === 11000) objects[1].get = function () { return 5; };
}));
console.log(results.join(","));
