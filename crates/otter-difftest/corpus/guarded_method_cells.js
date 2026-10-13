// Inherited methods read at hot call sites whose prototype proof fixes the
// holder slot: methods created fresh (young) and moved by collections while
// the code holds them, a prototype method redefined and deleted mid-loop, an
// instance whose [[Prototype]] changes, and polymorphic receivers.
const results = [];

function makeClass(tag) {
  // Each class gets fresh closures, allocated just before the hot loop.
  function Base(v) {
    this.v = v;
  }
  Base.prototype.step = function (x) {
    return this.v + x + tag;
  };
  Base.prototype.scale = function (x) {
    return this.step(x) * 2;
  };
  function Derived(v) {
    Base.call(this, v);
    this.w = v + 1;
  }
  Derived.prototype = Object.create(Base.prototype);
  Derived.prototype.constructor = Derived;
  Derived.prototype.step = function (x) {
    return this.w - x + tag;
  };
  return { Base, Derived };
}

function run(objects, n) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const o = objects[i % objects.length];
    sum = (sum + o.scale(i & 15) + o.step(i & 7)) % 1000003;
  }
  return sum;
}

// Fresh classes each round, with allocation churn so the closures move.
let churn = [];
for (let round = 0; round < 6; round++) {
  const { Base, Derived } = makeClass(round);
  const objects = [new Base(1), new Derived(2), new Base(3), new Derived(4)];
  let total = 0;
  for (let k = 0; k < 20; k++) {
    total = (total + run(objects, 2000)) % 1000003;
    for (let j = 0; j < 3000; j++) churn.push({ j, s: 'c' + j });
    if (churn.length > 30000) churn = [];
  }
  results.push(total);
}

// Redefine, then delete, a prototype method while the site is hot.
{
  const { Base, Derived } = makeClass(7);
  const objects = [new Base(5), new Derived(6)];
  let total = 0;
  for (let k = 0; k < 30; k++) {
    if (k === 10) Base.prototype.step = function (x) { return x * 3; };
    if (k === 20) delete Derived.prototype.step;
    total = (total + run(objects, 1000)) % 1000003;
  }
  results.push(total, objects[1].step(2), objects[0].scale(4));
}

// An instance moves to another prototype mid-loop.
{
  const a = makeClass(1);
  const b = makeClass(2);
  const o = new a.Base(10);
  let total = 0;
  for (let k = 0; k < 30; k++) {
    if (k === 15) Object.setPrototypeOf(o, b.Derived.prototype);
    if (k === 15) o.w = 40;
    total = (total + run([o, new a.Derived(3)], 1000)) % 1000003;
  }
  results.push(total);
}

// The holder gains an own property shadowing nothing, then a shadowing one.
{
  const { Base } = makeClass(3);
  const o = new Base(2);
  let total = 0;
  for (let k = 0; k < 30; k++) {
    if (k === 10) Base.prototype.extra = 1;
    if (k === 20) o.step = function (x) { return -x; };
    total = (total + run([o], 1000)) % 1000003;
  }
  results.push(total);
}

console.log(JSON.stringify(results));
