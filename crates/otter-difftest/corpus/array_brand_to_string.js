// Canonical Array/Proxy classification precedes observable tag lookup.
// Collecting getters retain boxed receivers and exact fallback source effects.
const objectTag = Object.prototype.toString;
const arrayTag = Array.prototype.toString;
let checks = 0, allocations = 0;
const retained = [];
function check(value, label) {
  if (!value) throw new Error(label);
  checks++;
}
function tag(value) { return objectTag.call(value); }
function fallback(value) { return arrayTag.call(value); }
function collect(receiver) {
  for (let i = 0; i < 64; i++) {
    retained[i & 7] = {receiver, child: [receiver, i]};
    allocations++;
  }
}
function throwsTypeError(action) {
  try { action(); } catch (error) { return error instanceof TypeError; }
  return false;
}

check(tag(Array.prototype) === "[object Array]", "canonical prototype tag");
const prototypeProxy = new Proxy(Array.prototype, {});
check(Array.isArray(prototypeProxy) && tag(prototypeProxy) === "[object Array]",
  "canonical prototype proxy tag");
const array = [], arrayProxy = new Proxy(array, {});
const nestedProxy = new Proxy(arrayProxy, {});
for (let i = 0; i < 10000; i++) tag(arrayProxy);
check(tag(array) === "[object Array]" && tag(nestedProxy) === "[object Array]",
  "actual and nested array proxy tags");
const unbranded = Object.create(Array.prototype);
check(!Array.isArray(unbranded) && tag(unbranded) === "[object Object]",
  "prototype identity is not inherited");
const cyclic = [];
cyclic[0] = cyclic;
check(tag(new Proxy(cyclic, {})) === "[object Array]", "cyclic array brand");
function callable() {}
class Constructor {}
check(tag(new Proxy(callable, {})) === "[object Function]", "callable proxy tag");
check(tag(new Proxy(Constructor, {})) === "[object Function]", "class proxy tag");
check(tag(new Proxy({}, {})) === "[object Object]", "nonarray proxy tag");

let tagGets = 0, exactReceiver = false;
const customTarget = [];
let customProxy;
Object.defineProperty(customTarget, Symbol.toStringTag, {
  configurable: true,
  get() {
    tagGets++;
    collect(this);
    exactReceiver = this === customProxy && retained[7].receiver === this;
    return "CustomArray";
  }
});
customProxy = new Proxy(customTarget, {});
check(tag(customProxy) === "[object CustomArray]" && exactReceiver && tagGets === 1,
  "collecting tag getter receives exact proxy");
Object.defineProperty(customTarget, Symbol.toStringTag, {value: 17});
check(tag(customProxy) === "[object Array]", "nonstring tag falls back to brand");

let revokedGets = 0;
const revoked = Proxy.revocable([], {});
const revokedOuter = new Proxy(revoked.proxy, {
  get() { revokedGets++; return "Forbidden"; }
});
revoked.revoke();
check(throwsTypeError(() => tag(revokedOuter)) && revokedGets === 0,
  "revoked nested target fails before outer tag trap");
check(throwsTypeError(() => Array.isArray(revokedOuter)), "canonical revoked exception");

let lateGets = 0;
const late = Proxy.revocable(Array.prototype, {
  get(target, key, receiver) {
    if (key === Symbol.toStringTag) {
      lateGets++;
      collect(receiver);
      late.revoke();
      return undefined;
    }
    return Reflect.get(target, key, receiver);
  }
});
check(tag(late.proxy) === "[object Array]" && lateGets === 1,
  "tag getter revocation cannot change earlier brand");
check(throwsTypeError(() => tag(late.proxy)), "subsequent revoked receiver throws");

const order = [];
const fallbackTarget = [];
let fallbackProxy;
Object.defineProperty(fallbackTarget, "join", {
  get() {
    order.push("join");
    collect(this);
    check(this === fallbackProxy, "collecting join getter receiver");
    return null;
  }
});
Object.defineProperty(fallbackTarget, Symbol.toStringTag, {
  get() {
    order.push("tag");
    collect(this);
    check(this === fallbackProxy && retained[7].child[0] === this,
      "fallback receiver remains rooted after join getter");
    return undefined;
  }
});
fallbackProxy = new Proxy(fallbackTarget, {});
check(fallback(fallbackProxy) === "[object Array]" && order.join() === "join,tag",
  "fallback getter order and array brand");

const fallbackLate = Proxy.revocable([], {
  get(target, key, receiver) {
    if (key === "join") return 0;
    if (key === Symbol.toStringTag) {
      collect(receiver);
      fallbackLate.revoke();
      return undefined;
    }
    return Reflect.get(target, key, receiver);
  }
});
check(fallback(fallbackLate.proxy) === "[object Array]",
  "fallback classifies before revoking tag getter");

let boxedGets = 0;
const savedStringTag = Object.getOwnPropertyDescriptor(String.prototype, Symbol.toStringTag);
Object.defineProperty(String.prototype, Symbol.toStringTag, {
  configurable: true,
  get: function () {
    "use strict";
    boxedGets++;
    collect(this);
    check(typeof this === "object" && this.valueOf() === "boxed",
      "primitive tag getter receives rooted boxed object");
    return "BoxedString";
  }
});
check(fallback("boxed") === "[object BoxedString]" && boxedGets === 1,
  "primitive ToObject precedes fallback tag getter");
if (savedStringTag) Object.defineProperty(String.prototype, Symbol.toStringTag, savedStringTag);
else delete String.prototype[Symbol.toStringTag];
check(tag("plain") === "[object String]", "restored primitive tag");

const token = {marker: 7};
let throwingGets = 0, caught;
Object.defineProperty(array, Symbol.toStringTag, {
  get() { throwingGets++; collect(this); throw token; }
});
try { tag(arrayProxy); } catch (error) { caught = error; }
check(caught === token && throwingGets === 1, "collecting tag throw occurs once");
let nullishGets = 0;
Object.defineProperty(Object.prototype, Symbol.toStringTag, {
  configurable: true,
  get() { nullishGets++; return undefined; }
});
check(tag(null) === "[object Null]" && tag(undefined) === "[object Undefined]"
  && nullishGets === 0, "nullish tags skip ToObject and tag lookup");
delete Object.prototype[Symbol.toStringTag];

const tagKey = Symbol.toStringTag;
const primitiveCases = [
  ["String", "plain", "String"],
  ["Boolean", true, "Boolean"],
  ["Number", 7, "Number"],
  ["Symbol", Symbol("token"), "Symbol"],
  ["BigInt", 7n, "BigInt"]
];
for (const entry of primitiveCases) {
  const name = entry[0], value = entry[1], expected = "[object " + entry[2] + "]";
  const saved = globalThis[name];
  try {
    globalThis[name] = {prototype: {[tagKey]: "Wrong"}};
    check(tag(value) === expected && fallback(value) === expected,
      "replacement global constructor cannot choose wrapper prototype " + name);
    globalThis[name] = null;
    check(tag(value) === expected && fallback(value) === expected,
      "null global constructor cannot prevent primitive boxing " + name);
  } finally {
    globalThis[name] = saved;
  }
}
const savedObject = Object;
try {
  globalThis.Object = function ReplacementObject() {};
  check(tag(new savedObject(7)) === "[object Number]",
    "active Object constructor identity survives global replacement");
  globalThis.Object = null;
  check(tag(new savedObject(7)) === "[object Number]",
    "active Object constructor identity survives null global");
} finally {
  globalThis.Object = savedObject;
}
console.log(JSON.stringify({checks, allocations, tagGets, lateGets, boxedGets,
  revokedGets, throwingGets, order}));
