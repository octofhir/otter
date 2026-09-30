// `f.call(this, ...)` sites once the callers are hot: the generated site calls
// the receiver directly while `Function.prototype.call` is intact, and every
// broken assumption (another receiver, an own `call`, a replaced intrinsic,
// a non-closure receiver) must still produce the interpreter's result.

function Base(v) {
  this.v = v;
  this.tag = "base";
}
function Derived(v) {
  Base.call(this, v);
  this.w = v * 2;
}
function sloppyThis() {
  return this === globalThis ? "global" : typeof this;
}
function strictThis() {
  "use strict";
  return this === undefined ? "undefined" : typeof this;
}
function sum3(a, b, c) {
  return this.base + a + b + (c === undefined ? 0 : c);
}

function viaCall(f, self, a, b) {
  return f.call(self, a, b);
}
function noArgs(f) {
  return f.call();
}

let total = 0;
let shapes = "";
for (let i = 0; i < 5000; i++) {
  const d = new Derived(i);
  total += d.v + d.w;
  total += viaCall(sum3, { base: 1 }, i, 2);
  if (i % 10000 === 0) shapes += d.tag + noArgs(sloppyThis) + noArgs(strictThis) + ",";
}
console.log(total, shapes);

// Another closure at a hot monomorphic site.
function other(a, b) {
  return this.base * 1000 + a - b;
}
console.log(viaCall(other, { base: 3 }, 5, 1), viaCall(sum3, { base: 2 }, 1, 1));

// Non-closure receivers: bound, native, arrow, class method, object with `call`.
const bound = sum3.bind({ base: 100 });
console.log(viaCall(bound, { base: 1 }, 1, 2));
console.log(viaCall(Math.max, null, 4, 9));
console.log(viaCall((a, b) => a * b, { base: 0 }, 6, 7));
class K {
  m(a, b) {
    return this.base - a - b;
  }
}
console.log(viaCall(K.prototype.m, { base: 50 }, 5, 5));
console.log(viaCall({ call: (self, a, b) => "own:" + self.base + a + b }, { base: 7 }, 1, 2));
try {
  viaCall(K, {}, 1, 2);
} catch (e) {
  console.log("class", e instanceof TypeError);
}

// An own `call` on one closure only.
function withOwn(a, b) {
  return a + b;
}
withOwn.call = function (self, a, b) {
  return "shadowed " + (a - b);
};
console.log(viaCall(withOwn, null, 10, 3), viaCall(sum3, { base: 0 }, 10, 3));

// Exceptions thrown through a direct f.call, inside and outside handlers.
function thrower(n) {
  if (n > 4990) throw new RangeError("n=" + n);
  return n;
}
function guarded(n) {
  try {
    return thrower.call(null, n);
  } catch (e) {
    return e.message;
  }
}
let caught = "";
let acc = 0;
for (let i = 0; i < 5000; i++) {
  const r = guarded(i);
  if (typeof r === "string") caught = r;
  else acc += r;
}
console.log(acc, caught);
try {
  for (let i = 0; i < 5000; i++) viaCall(thrower, null, i);
} catch (e) {
  console.log("outer", e.message);
}

// Replacing the intrinsic after the sites are optimized, then restoring it.
const originalCall = Function.prototype.call;
Function.prototype.call = function (self, a, b) {
  return "replaced:" + this.name + ":" + a + ":" + b;
};
console.log(viaCall(sum3, { base: 1 }, 2, 3), new Derived(4).v);
Function.prototype.call = originalCall;
let again = 0;
for (let i = 0; i < 2500; i++) again += viaCall(sum3, { base: 1 }, i, 0);
console.log(again);

// A getter for `call` on Function.prototype.
Object.defineProperty(Function.prototype, "call", {
  configurable: true,
  get() {
    return function (self, a) {
      return "getter:" + a;
    };
  },
});
console.log(viaCall(sum3, null, 8, 0));
Object.defineProperty(Function.prototype, "call", {
  configurable: true,
  writable: true,
  value: originalCall,
});
console.log(viaCall(sum3, { base: 5 }, 1, 1));

// The loaded form: effectful arguments or captured receivers make the
// compiler load `f.call` first and call it with `f` as `this`.
function id(x) {
  return x;
}
function viaLoaded(f, self, a) {
  return f.call(self, id(a), 2);
}
const makeSub = (parent, k) => {
  function Sub(x) {
    parent.call(this, k);
    this.x = x;
  }
  return Sub;
};
const Sub = makeSub(Base, 7);
let loaded = 0;
for (let i = 0; i < 5000; i++) {
  loaded += viaLoaded(sum3, { base: 1 }, i);
  loaded += new Sub(i).v;
}
console.log(loaded);
console.log(viaLoaded(other, { base: 2 }, 9), viaLoaded(bound, null, 1), viaLoaded(Math.min, null, 3));
console.log(viaLoaded(withOwn, null, 4), new (makeSub(Derived, 3))(1).w);
Function.prototype.call = function (self, a) {
  return "loaded-replaced:" + a;
};
console.log(viaLoaded(sum3, null, 5), new Sub(1).v);
Function.prototype.call = originalCall;
let loadedAgain = 0;
for (let i = 0; i < 2500; i++) loadedAgain += viaLoaded(sum3, { base: 0 }, i);
console.log(loadedAgain);
