// Hot stores into own properties of functions: counters on plain functions
// and classes, built-in natives, class statics read through a subclass, and
// properties that become read-only, accessors or deleted mid-loop, frozen
// functions, swapped prototypes and the names a function synthesizes.
"use strict";
function Stats() {}
Stats.calls = 0;
Stats.max = 0;
class Counter { static hits = 0; static last = null; }
class Child extends Counter {}
Array.extraCount = 0;

function bump(f, n, hook) {
  let seen = 0;
  for (let i = 0; i < n; i++) {
    if (hook) hook(i);
    ++f.calls;
    f.max = Math.max(f.max, i & 255);
    seen += f.calls;
  }
  return seen + ":" + f.calls + ":" + f.max;
}

const out = [];
out.push(bump(Stats, 6000, null));

// Static fields on a class whose subclass inherits them through its own
// [[Prototype]]; the subclass must observe every write.
let inherited = 0;
for (let i = 0; i < 6000; i++) {
  Counter.hits++;
  Counter.last = i;
  inherited += Child.hits + Child.last;
}
out.push(inherited, Counter.hits, Object.hasOwn(Child, "hits"));

// A built-in native's bag.
for (let i = 0; i < 5000; i++) Array.extraCount += 2;
out.push(Array.extraCount);

// Read-only mid-loop: strict assignment throws.
function Locked() {}
Locked.calls = 0;
Locked.max = 0;
let thrown = 0;
try {
  bump(Locked, 6000, (i) => {
    if (i === 3000) Object.defineProperty(Locked, "calls", { writable: false });
  });
} catch (e) { thrown++; }
out.push(thrown, Locked.calls);

// An accessor replaces the data property mid-loop.
function Watched() {}
Watched.calls = 0;
Watched.max = 0;
let setterRuns = 0;
out.push(bump(Watched, 6000, (i) => {
  if (i === 2500) {
    let hidden = Watched.calls;
    Object.defineProperty(Watched, "calls", {
      configurable: true,
      get() { return hidden; },
      set(v) { setterRuns++; hidden = v * 1; },
    });
  }
}), setterRuns);

// Deleted and re-added, then frozen.
function Fickle() {}
Fickle.calls = 0;
Fickle.max = 0;
let frozenThrows = 0;
try {
  bump(Fickle, 9000, (i) => {
    if (i === 2000) { delete Fickle.calls; Fickle.calls = 100; }
    if (i === 6000) Object.freeze(Fickle);
  });
} catch (e) { frozenThrows++; }
out.push(Fickle.calls, frozenThrows);

// A swapped [[Prototype]] does not affect own data stores.
function Swapped() {}
Swapped.calls = 0;
Swapped.max = 0;
out.push(bump(Swapped, 6000, (i) => {
  if (i === 3000) Object.setPrototypeOf(Swapped, { inheritedOnly: 1 });
}));

// Names a function synthesizes: `name` and `length` are read-only.
function Named() {}
let nameThrows = 0;
for (let i = 0; i < 3000; i++) {
  try { Named.name = "x" + i; } catch (e) { nameThrows++; }
  try { Named.length = i; } catch (e) { nameThrows++; }
}
let protoSwaps = 0;
for (let i = 0; i < 3000; i++) {
  Named.prototype = { i };
  protoSwaps += Named.prototype.i;
}
out.push(Named.name, Named.length, nameThrows, protoSwaps);
console.log(out.join(","));
