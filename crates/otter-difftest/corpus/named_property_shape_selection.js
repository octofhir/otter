"use strict";

function methodA(x) { return x + 40; }
function methodB(x) { return x + 70; }
function replacement(x) { return x + 100; }

class A {
  constructor() {
    this.receiverProof = 1;
    this.own = 11;
    this.prototype = 7;
    this.method = methodA;
  }
}

class B {
  constructor() {
    this.receiverProof = 2;
    for (let i = 0; i < 70; ++i) this["p" + i] = i;
    this.own = 29;
    this.prototype = 9;
    this.method = methodB;
  }
}

A.prototype.value = 11;
for (let i = 0; i < 70; ++i) B.prototype["h" + i] = i;
B.prototype.value = 29;

function F() {}
const a = new A(), b = new B(), changing = new A();
const exotic = [new Map(), new Set(), "abc", F];

function own(o) { return o.own; }
function inherited(o) { return o.value; }
function constructorOf(o) { return o.constructor; }
function prototypeOf(o) { return o.prototype; }
function methodOf(o) { return o.method; }
function ownA(o) { const x = o.receiverProof; return x + own(o); }
function ownB(o) { const x = o.receiverProof; return x + own(o); }
function inheritedA(o) { const x = o.receiverProof; return x + inherited(o); }
function inheritedB(o) { const x = o.receiverProof; return x + inherited(o); }
function constructorA(o) { const x = o.receiverProof; return constructorOf(o) === A ? x : -x; }
function constructorB(o) { const x = o.receiverProof; return constructorOf(o) === B ? x : -x; }
function prototypeA(o) { const x = o.receiverProof; return x + prototypeOf(o); }
function prototypeB(o) { const x = o.receiverProof; return x + prototypeOf(o); }

let entries = 0, effects = 0, getters = 0, mutation = null, replaceMethod = false;
function effectA(o) { if (mutation !== null) { ++effects; mutation(o); } }
function effectB(o) { if (mutation !== null) { ++effects; mutation(o); } }
function argumentA(o) { if (replaceMethod) { ++effects; o.method = replacement; } return 3; }
function argumentB() { return 3; }

function callA(o, argument) {
  const x = o.receiverProof;
  const fn = methodOf(o);
  return x + fn(argument(o));
}

function mutateA(o, effect) {
  ++entries;
  const x = o.receiverProof;
  effect(o);
  return x + inherited(o);
}

function trackedA(o) { ++entries; const x = o.receiverProof; return x + inherited(o); }

for (let i = 0; i < 6000; ++i) {
  const o = (i & 1) ? a : b;
  own(o);
  inherited(o);
  constructorOf(o);
  prototypeOf(o);
  methodOf(o);
  constructorOf(exotic[i & 3]);
  prototypeOf(F);
}

for (let i = 0; i < 6000; ++i) {
  ownA(a);
  ownB(b);
  inheritedA(a);
  inheritedB(b);
  constructorA(a);
  constructorB(b);
  prototypeA(a);
  prototypeB(b);
  callA(a, (i & 1) ? argumentA : argumentB);
  mutateA(changing, (i & 1) ? effectA : effectB);
  trackedA(a);
}

const result = [
  ownA(a), ownB(b), inheritedA(a), inheritedB(b),
  constructorA(a), constructorB(b), prototypeA(a), prototypeB(b),
  prototypeOf(F) === F.prototype
];

replaceMethod = true;
effects = 0;
result.push(callA(a, argumentA), effects, a.method === replacement);
replaceMethod = false;

function observe(action) {
  mutation = action;
  entries = effects = getters = 0;
  const value = mutateA(changing, effectA);
  result.push(value, entries, effects, getters);
  mutation = null;
}

observe(() => { A.prototype.value = 33; });
observe(() => {
  Object.defineProperty(A.prototype, "value", {
    configurable: true,
    get() { ++getters; return 101; }
  });
});
observe(o => {
  Object.defineProperty(o, "value", {
    configurable: true,
    writable: true,
    value: 55
  });
});

entries = getters = 0;
const unknown = {
  get receiverProof() { ++getters; return 5; },
  get value() { ++getters; return 13; }
};
result.push(trackedA(unknown), entries, getters);
console.log(JSON.stringify(result));
