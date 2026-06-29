// Polymorphic method-dispatch correctness + perf workload.
//
// One call site (`obj.run(acc)`) sees several sibling-class receiver shapes, so
// the baseline JIT bakes a polymorphic inline guard chain (most-frequent-first)
// instead of collapsing to the per-call method bridge. This script is primarily
// a differential gate: `interp`, `jit`, and `jit-osr` must agree byte-for-byte,
// which pins the guard-chain dispatch, the megamorphic fallback, and prototype
// method-reassignment invalidation. Deterministic output (no timing).

// 4 sibling classes sharing `run` — within the inline polymorphic width.
class A { constructor() { this.k = 1; } run(x) { return x + this.k; } }
class B { constructor() { this.k = 2; } run(x) { return x + this.k * 2; } }
class C { constructor() { this.k = 3; } run(x) { return x + this.k * 3; } }
class D { constructor() { this.k = 4; } run(x) { return x + this.k * 4; } }

function drive(arr, iters) {
  let acc = 0;
  for (let it = 0; it < iters; it++) {
    for (let i = 0; i < arr.length; i++) acc = arr[i].run(acc);
  }
  return acc;
}

const poly = [];
for (let i = 0; i < 1200; i++) {
  const m = i & 3;
  poly.push(m === 0 ? new A() : m === 1 ? new B() : m === 2 ? new C() : new D());
}
let polySum = 0;
for (let rep = 0; rep < 40; rep++) polySum += drive(poly, 60);

// 6 shapes through one site exceeds the inline width -> megamorphic -> bridge.
class E { run(x) { return x + 5; } }
class F { run(x) { return x + 6; } }
const mega = [];
const klass = [A, B, C, D, E, F];
for (let i = 0; i < 1800; i++) mega.push(new klass[i % 6]());
let megaSum = 0;
for (let rep = 0; rep < 40; rep++) megaSum += drive(mega, 30);

// Prototype method reassignment mid-flight must invalidate the inline guard
// (the chain re-resolves the method slot every call), so the new body wins.
class G { run(x) { return x + 1; } }
class H { run(x) { return x + 2; } }
const re = [];
for (let i = 0; i < 1000; i++) re.push(i & 1 ? new G() : new H());
let reSum = 0;
for (let rep = 0; rep < 200; rep++) {
  reSum += drive(re, 5);
  if (rep === 100) G.prototype.run = function (x) { return x + 50; };
}

console.log("poly=" + polySum + " mega=" + megaSum + " reassign=" + reSum);
