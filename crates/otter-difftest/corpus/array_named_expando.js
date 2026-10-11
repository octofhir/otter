// Array named storage uses ordinary descriptors with the original receiver.
let checks = 0;
function check(value, label) { if (!value) throw new Error(label); checks++; }
const own = (value, key) => Object.prototype.hasOwnProperty.call(value, key);
const symbol = Symbol("named"), laterSymbol = Symbol("later");
const array = [3, , 5];
array.first = 7;
array["01"] = 11;
array["4294967295"] = 13;
array[symbol] = 17;
check(array.length === 3 && !(1 in array), "named keys preserve length and holes");
check(Object.keys(array).join() === "0,2,first,01,4294967295", "string key order");
const keys = Reflect.ownKeys(array);
check(keys.slice(0, 6).join() === "0,2,length,first,01,4294967295" && keys[6] === symbol,
  "own keys put symbols last");
delete array.first;
array.first = 19;
array[laterSymbol] = 23;
check(Object.keys(array).join() === "0,2,01,4294967295,first", "delete and recreate order");
check(Object.getOwnPropertySymbols(array)[1] === laterSymbol, "symbol birth order");
const descriptor = Object.getOwnPropertyDescriptor(array, "first");
check(descriptor.value === 19 && descriptor.writable && descriptor.enumerable && descriptor.configurable,
  "default named descriptor");
Object.defineProperty(array, "hidden", {value: 29, writable: true});
check(own(array, "hidden") && !Object.keys(array).includes("hidden"), "non-enumerable named descriptor");
check(delete array.hidden === false && array.hidden === 29, "non-configurable deletion");
let getReceiver, setReceiver, stored = 0;
Object.defineProperty(array, "access", {
  configurable: true, enumerable: true,
  get() { getReceiver = this; return stored; },
  set(value) { setReceiver = this; stored = value; }
});
array.access = 31;
check(array.access === 31 && getReceiver === array && setReceiver === array, "own accessor receiver");
const proxy = new Proxy(array, {});
proxy.access = 37;
check(proxy.access === 37 && getReceiver === proxy && setReceiver === proxy, "proxy accessor receiver");
const target = {tag: 41};
check(Reflect.set(array, "first", target, proxy) && array.first === target,
  "named overwrite through proxy receiver");
let inheritedCalls = 0, inheritedReceiver;
Object.defineProperty(Array.prototype, "expandoInheritedSetter", {
  configurable: true, set(value) { inheritedCalls++; inheritedReceiver = this; stored = value; }
});
const inherited = [];
inherited.expandoInheritedSetter = 43;
check(inheritedCalls === 1 && inheritedReceiver === inherited && stored === 43
  && !own(inherited, "expandoInheritedSetter"), "actual array prototype setter");
delete Array.prototype.expandoInheritedSetter;
const arrayPrototype = [];
Object.defineProperty(arrayPrototype, "arrayInheritedSetter", {
  configurable: true, set(value) { inheritedCalls++; inheritedReceiver = this; stored = value; }
});
const arrayChild = [];
Object.setPrototypeOf(arrayChild, arrayPrototype);
arrayChild.arrayInheritedSetter = 45;
check(inheritedCalls === 2 && inheritedReceiver === arrayChild && stored === 45
  && !own(arrayChild, "arrayInheritedSetter"), "actual array as inherited setter holder");
const proto = Object.create(Array.prototype);
proto.inherited = 47;
Object.setPrototypeOf(inherited, proto);
check(inherited.inherited === 47 && Object.getPrototypeOf(inherited) === proto, "explicit prototype");
inherited.inherited = 53;
check(inherited.inherited === 53 && proto.inherited === 47, "own shadows inherited storage");
delete inherited.inherited;
check(inherited.inherited === 47, "deletion reveals actual prototype");
const nullPrototype = [];
Object.setPrototypeOf(nullPrototype, null);
check(!(Symbol.iterator in nullPrototype) && !own(nullPrototype, Symbol.iterator),
  "null prototype does not invent inherited iterator");
check(Object.getPrototypeOf(nullPrototype) === null
  && nullPrototype[Symbol.iterator] === undefined
  && !Reflect.has(nullPrototype, Symbol.iterator), "null prototype remains explicit on every lookup");
check(Reflect.setPrototypeOf(nullPrototype, Array.prototype)
  && nullPrototype[Symbol.iterator] === Array.prototype[Symbol.iterator], "explicit prototype restores inherited iterator");
for (const literalPrototype of [[], new Map(), new Set(), function LiteralPrototype() {}, class LiteralPrototypeClass {}]) {
  const literal = {__proto__: literalPrototype, own: 57};
  check(Object.getPrototypeOf(literal) === literalPrototype && literal.own === 57,
    "literal prototype retains every Object-kind identity");
}
const primitivePrototypeLiteral = {__proto__: 17, own: 58};
check(Object.getPrototypeOf(primitivePrototypeLiteral) === Object.prototype && primitivePrototypeLiteral.own === 58,
  "primitive literal prototype is ignored by its source operation");
class ActualPrototypeBase { static receiver() { return this; } }
class ActualPrototypeDerived extends ActualPrototypeBase {
  static parentPrototype() { return super.prototype; }
  static parentName() { return super.name; }
  static parentReceiver() { return super.receiver(); }
}
check(Object.getPrototypeOf(ActualPrototypeDerived) === ActualPrototypeBase
  && ActualPrototypeDerived.parentPrototype() === ActualPrototypeBase.prototype
  && ActualPrototypeDerived.parentName() === "ActualPrototypeBase"
  && ActualPrototypeDerived.parentReceiver() === ActualPrototypeDerived,
  "static super retains actual parent identity and receiver");
class ReplacementPrototypeBase { static receiver() { return this; } }
check(Reflect.setPrototypeOf(ActualPrototypeDerived, ReplacementPrototypeBase)
  && Object.getPrototypeOf(ActualPrototypeDerived) === ReplacementPrototypeBase
  && ActualPrototypeDerived.parentPrototype() === ReplacementPrototypeBase.prototype
  && ActualPrototypeDerived.parentName() === "ReplacementPrototypeBase"
  && ActualPrototypeDerived.parentReceiver() === ActualPrototypeDerived,
  "class prototype mutation publishes the same actual parent on both paths");
function ArrayPrototypeBase() {}
const actualArrayBasePrototype = [];
ArrayPrototypeBase.prototype = actualArrayBasePrototype;
class ActualArrayPrototypeDerived extends ArrayPrototypeBase {}
check(Object.getPrototypeOf(ActualArrayPrototypeDerived.prototype) === actualArrayBasePrototype,
  "class instance prototype accepts an actual Array parent prototype");
for (const [first, second] of [[[], []], [{}, []], [[], {}], [function cycleFunction() {}, []],
  [[], function otherCycleFunction() {}], [[], new Map()], [[], new Set()],
  [[], class CycleClass {}], [[], (function boundCycleFunction() {}).bind(null)]]) {
  const secondPrototype = Object.getPrototypeOf(second);
  check(Reflect.setPrototypeOf(first, second) && !Reflect.setPrototypeOf(second, first)
    && Object.getPrototypeOf(first) === second && Object.getPrototypeOf(second) === secondPrototype,
    "ordinary prototype cycle is refused across physical receiver types");
}
const proxyCycleTarget = [];
let proxyCyclePrototypeCalls = 0;
const proxyCycleLink = new Proxy(proxyCycleTarget, {
  getPrototypeOf() { proxyCyclePrototypeCalls++; return null; }
});
check(Reflect.setPrototypeOf(proxyCycleTarget, proxyCycleLink) && proxyCyclePrototypeCalls === 0,
  "ordinary cycle detection stops at proxy without invoking its trap");
Reflect.setPrototypeOf(proxyCycleTarget, Array.prototype);
const closedPrototypeClass = class ClosedPrototypeClass {};
const closedClassPrototype = Object.getPrototypeOf(closedPrototypeClass);
Object.preventExtensions(closedPrototypeClass);
check(!Reflect.setPrototypeOf(closedPrototypeClass, Array.prototype)
  && Object.getPrototypeOf(closedPrototypeClass) === closedClassPrototype,
  "refused class prototype mutation has no early constructor publication");
const closed = [];
closed.value = 59;
Object.preventExtensions(closed);
check(Reflect.set(closed, "value", 61) && closed.value === 61, "closed writable overwrite");
check(!Reflect.set(closed, "newKey", 67) && !own(closed, "newKey"), "closed append refused");
check(!Reflect.defineProperty(closed, symbol, {value: 71}), "closed symbol append refused");
const sealed = [73];
sealed.named = 79;
sealed[symbol] = 83;
Object.seal(sealed);
check(Object.isSealed(sealed) && Reflect.set(sealed, "named", 89)
  && !Reflect.deleteProperty(sealed, "named") && !Reflect.deleteProperty(sealed, symbol), "sealed descriptors");
Object.freeze(sealed);
check(Object.isFrozen(sealed) && !Reflect.set(sealed, "named", 97)
  && !Reflect.set(sealed, symbol, 101) && !Reflect.set(sealed, "0", 103), "frozen named and indexed storage");
for (const key of ["named", symbol, "0"]) {
  let invariantError = false;
  const wrong = new Proxy(sealed, {get() { return -1; }});
  try { Reflect.get(wrong, key); } catch (error) { invariantError = error instanceof TypeError; }
  check(invariantError, "proxy get checks frozen array descriptor");
}
let nestedDescriptorCalls = 0;
const nestedTarget = new Proxy(sealed, {
  getOwnPropertyDescriptor(target, key) {
    nestedDescriptorCalls++;
    return Reflect.getOwnPropertyDescriptor(target, key);
  }
});
const nestedGet = new Proxy(nestedTarget, {get() { return 97; }});
let nestedInvariantError = false;
try { nestedGet.named; } catch (error) { nestedInvariantError = error instanceof TypeError; }
check(nestedInvariantError && nestedDescriptorCalls === 1,
  "nested proxy descriptor trap runs before invariant rejection");
let revokingGet;
const revocableGet = Proxy.revocable(sealed, {
  get() { revokingGet(); return 89; }
});
revokingGet = revocableGet.revoke;
check(revocableGet.proxy.named === 89, "get invariant keeps target captured before revocation");
const closedHasTarget = [];
closedHasTarget.configurable = 91;
Object.preventExtensions(closedHasTarget);
for (const [target, key] of [[sealed, "named"], [sealed, symbol], [closedHasTarget, "configurable"]]) {
  let revokeHas, hasCalls = 0, hasError = false;
  const revocableHas = Proxy.revocable(target, {
    has() {
      hasCalls++;
      const pressure = [];
      for (let i = 0; i < 64; i++) pressure.push({i, child: [i]});
      revokeHas();
      return false;
    }
  });
  revokeHas = revocableHas.revoke;
  try { Reflect.has(revocableHas.proxy, key); } catch (error) { hasError = error instanceof TypeError; }
  check(hasError && hasCalls === 1, "has invariant keeps captured target and key through revocation");
}
let revokeAbsentHas, absentHasCalls = 0;
const revocableAbsentHas = Proxy.revocable([], {
  has() { absentHasCalls++; revokeAbsentHas(); return false; }
});
revokeAbsentHas = revocableAbsentHas.revoke;
check(!Reflect.has(revocableAbsentHas.proxy, symbol) && absentHasCalls === 1,
  "revoked has trap may report an absent key from its captured target");
const nestedHasKey = Symbol("nested has key"), nestedHasTarget = [];
nestedHasTarget[nestedHasKey] = {marker: 93};
Object.preventExtensions(nestedHasTarget);
const nestedHasOrder = [];
const nestedHasInner = new Proxy(nestedHasTarget, {
  getOwnPropertyDescriptor(target, key) {
    nestedHasOrder.push("own");
    const pressure = [];
    for (let i = 0; i < 64; i++) pressure.push({i, child: [i]});
    return Reflect.getOwnPropertyDescriptor(target, key);
  },
  isExtensible(target) { nestedHasOrder.push("extensible"); return Reflect.isExtensible(target); }
});
let revokeNestedHas, nestedHasError = false;
const nestedHasOuter = Proxy.revocable(nestedHasInner, {
  has() { nestedHasOrder.push("has"); revokeNestedHas(); return false; }
});
revokeNestedHas = nestedHasOuter.revoke;
try { Reflect.has(nestedHasOuter.proxy, nestedHasKey); } catch (error) { nestedHasError = error instanceof TypeError; }
check(nestedHasError && nestedHasOrder.join() === "has,own,extensible",
  "captured has target traverses nested descriptor and extensibility traps once");
let revokingDescriptor;
const revocableDescriptor = Proxy.revocable(sealed, {
  getOwnPropertyDescriptor(target, key) {
    revokingDescriptor();
    return Reflect.getOwnPropertyDescriptor(target, key);
  }
});
revokingDescriptor = revocableDescriptor.revoke;
const outerDescriptor = new Proxy(revocableDescriptor.proxy, {get() { return 89; }});
check(outerDescriptor.named === 89, "nested descriptor invariant keeps pre-trap target");
const arrayDescriptor = [];
arrayDescriptor.value = {marker: 103};
arrayDescriptor.configurable = true;
const functionDescriptor = function descriptorCarrier() {};
functionDescriptor.value = {marker: 105};
functionDescriptor.configurable = true;
for (const descriptorCarrier of [arrayDescriptor, functionDescriptor]) {
  const descriptorProxy = new Proxy({}, {getOwnPropertyDescriptor() { return descriptorCarrier; }});
  check(Object.getOwnPropertyDescriptor(descriptorProxy, "virtual").value === descriptorCarrier.value,
    "descriptor trap accepts every Object kind");
}
const descriptorTarget = [];
descriptorTarget.named = {marker: 106};
const descriptorInner = new Proxy(descriptorTarget, {
  getOwnPropertyDescriptor(target, key) {
    const pressure = [];
    for (let i = 0; i < 64; i++) pressure.push({i, child: [i]});
    return Reflect.getOwnPropertyDescriptor(target, key);
  }
});
const descriptorOuter = new Proxy(descriptorInner, {
  getOwnPropertyDescriptor() {
    return {value: {marker: 107}, writable: true, enumerable: true, configurable: true};
  }
});
check(Object.getOwnPropertyDescriptor(descriptorOuter, "named").value.marker === 107,
  "descriptor payload remains rooted through nested target trap");
const proxyDescriptorOrder = [];
const orderedDescriptorTarget = new Proxy({}, {
  getOwnPropertyDescriptor() { proxyDescriptorOrder.push("own"); return undefined; },
  isExtensible(target) { proxyDescriptorOrder.push("extensible"); return Reflect.isExtensible(target); }
});
const orderedDescriptor = new Proxy(orderedDescriptorTarget, {
  getOwnPropertyDescriptor() {
    proxyDescriptorOrder.push("trap");
    return {configurable: true, get value() { proxyDescriptorOrder.push("value"); return 108; }};
  }
});
check(Object.getOwnPropertyDescriptor(orderedDescriptor, "newKey").value === 108
  && proxyDescriptorOrder.join() === "trap,own,extensible,value", "proxy descriptor conversion order");
const closedDescriptorArray = [];
Object.preventExtensions(closedDescriptorArray);
let closedDescriptorError = false;
try {
  Object.getOwnPropertyDescriptor(new Proxy(closedDescriptorArray, {
    getOwnPropertyDescriptor() { return {value: 109, configurable: true}; }
  }), "missing");
} catch (error) { closedDescriptorError = error instanceof TypeError; }
check(closedDescriptorError, "proxy cannot invent own key on non-extensible array");
let nullDescriptorError = false;
try {
  Object.getOwnPropertyDescriptor(new Proxy([], {getOwnPropertyDescriptor() { return null; }}), "missing");
} catch (error) { nullDescriptorError = error instanceof TypeError; }
check(nullDescriptorError, "null descriptor trap result is invalid");
let absentDescriptorError = false;
try {
  Object.getOwnPropertyDescriptor(new Proxy(sealed, {
    getOwnPropertyDescriptor() { return undefined; }
  }), "named");
} catch (error) { absentDescriptorError = error instanceof TypeError; }
check(absentDescriptorError, "undefined descriptor cannot hide frozen array key");
let revokeExtensible;
const selfRevokingExtensible = Proxy.revocable([], {
  isExtensible(target) { revokeExtensible(); return Reflect.isExtensible(target); }
});
revokeExtensible = selfRevokingExtensible.revoke;
check(Reflect.isExtensible(selfRevokingExtensible.proxy), "isExtensible retains target through revocation");
let revokePreventExtensions;
const preventedTarget = [];
preventedTarget.named = 109;
const selfRevokingPreventExtensions = Proxy.revocable(preventedTarget, {
  preventExtensions(target) { revokePreventExtensions(); return Reflect.preventExtensions(target); }
});
revokePreventExtensions = selfRevokingPreventExtensions.revoke;
check(Reflect.preventExtensions(selfRevokingPreventExtensions.proxy)
  && !Reflect.isExtensible(preventedTarget) && preventedTarget.named === 109,
  "preventExtensions retains target through revocation");
for (const requestedPrototype of [null, Array.prototype]) {
  let revokeSetPrototype, setPrototypeCalls = 0, setPrototypeError = false, setPrototypeResult;
  const setPrototypeTarget = [];
  Object.preventExtensions(setPrototypeTarget);
  const revocableSetPrototype = Proxy.revocable(setPrototypeTarget, {
    setPrototypeOf() { setPrototypeCalls++; revokeSetPrototype(); return true; }
  });
  revokeSetPrototype = revocableSetPrototype.revoke;
  try { setPrototypeResult = Reflect.setPrototypeOf(revocableSetPrototype.proxy, requestedPrototype); }
  catch (error) { setPrototypeError = error instanceof TypeError; }
  check(setPrototypeCalls === 1 && (requestedPrototype === null ? setPrototypeError
    : setPrototypeResult === true && !setPrototypeError), "setPrototypeOf invariant retains captured target after revocation");
}
let revokeNestedExtensible;
const nestedExtensible = Proxy.revocable([], {
  isExtensible(target) { revokeNestedExtensible(); return Reflect.isExtensible(target); }
});
revokeNestedExtensible = nestedExtensible.revoke;
const outerExtensible = new Proxy(nestedExtensible.proxy, {
  getOwnPropertyDescriptor() { return {value: 110, configurable: true}; }
});
check(Object.getOwnPropertyDescriptor(outerExtensible, "virtual").value === 110,
  "descriptor validation retains target during nested extensibility trap");
const symbolConversion = [], firstConversion = Symbol("first"), secondConversion = Symbol("second");
symbolConversion[firstConversion] = 111;
symbolConversion[secondConversion] = 112;
Object.defineProperty(symbolConversion, firstConversion, {
  configurable: true, enumerable: false, get() { return 113; }
});
check(Object.getOwnPropertySymbols(symbolConversion)[0] === firstConversion
  && symbolConversion[firstConversion] === 113, "symbol accessor conversion retains birth position");
Object.defineProperty(symbolConversion, firstConversion, {value: 114, writable: true});
Object.freeze(symbolConversion);
const firstConversionDescriptor = Object.getOwnPropertyDescriptor(symbolConversion, firstConversion);
check(Object.getOwnPropertySymbols(symbolConversion)[1] === secondConversion
  && firstConversionDescriptor.value === 114 && !firstConversionDescriptor.writable
  && !firstConversionDescriptor.enumerable && !firstConversionDescriptor.configurable,
  "symbol data conversion and freeze retain order and attributes");
class PrivateArray extends Array {
  #value = 37;
  value() { return this.#value; }
  update() { this.#value = 38; }
}
const privateArray = new PrivateArray();
Object.freeze(privateArray);
privateArray.update();
check(privateArray.value() === 38 && Reflect.ownKeys(privateArray).join() === "length",
  "freeze and own keys exclude private fields");
const descriptors = [];
descriptors.length = 3;
descriptors[2] = {value: 107, enumerable: true};
Object.defineProperty(descriptors, "hidden", {value: {value: 109}, enumerable: false});
descriptors.named = {value: 113, enumerable: true};
descriptors[symbol] = {value: 127, enumerable: true};
const defined = {};
Object.defineProperties(defined, descriptors);
check(!own(defined, "0") && !own(defined, "1") && !own(defined, "hidden")
  && defined[2] === 107 && defined.named === 113 && defined[symbol] === 127,
  "descriptor array holes, enumerability and symbols");
const batchTarget = {};
const conversionOrder = [];
Object.defineProperties(batchTarget, {
  get first() { conversionOrder.push("first"); return {value: {tag: 129}, enumerable: true}; },
  get second() {
    conversionOrder.push("second");
    check(!own(batchTarget, "first"), "prepare every descriptor before defining");
    return {value: 130, enumerable: true};
  }
});
check(conversionOrder.join() === "first,second" && batchTarget.first.tag === 129
  && batchTarget.second === 130, "prepared descriptor values and order");
const failedTarget = {};
let descriptorThrow = "";
try {
  Object.defineProperties(failedTarget, {
    first: {value: 131}, get second() { throw "descriptor"; }
  });
} catch (error) { descriptorThrow = error; }
check(descriptorThrow === "descriptor" && !own(failedTarget, "first"),
  "descriptor conversion throw precedes every definition");
const hot = [];
hot.named = {value: 131};
hot[symbol] = 137;
function readNamed(receiver) { return receiver.named; }
function writeNamed(receiver, value) { receiver.named = value; return receiver.named; }
function readSymbol(receiver) { return receiver[symbol]; }
for (let i = 0; i < 6000; i++) { readNamed(hot); writeNamed(hot, hot.named); readSymbol(hot); }
const young = {value: 139};
check(writeNamed(hot, young) === young && readNamed(hot).value === 139 && readSymbol(hot) === 137,
  "hot named and symbol access");
let getterCalls = 0;
Object.defineProperty(hot, "named", {configurable: true, get() { getterCalls++; return young; }});
check(readNamed(hot) === young && getterCalls === 1, "descriptor transition invalidates hit");
delete hot.named;
check(readNamed(hot) === undefined, "deleted hot slot");
hot.named = young;
hot.self = hot;
const retained = [];
for (let i = 0; i < 1024; i++) retained.push({i, data: [i]});
check(hot.self === hot && readNamed(hot) === young && retained[1023].data[0] === 1023,
  "array bag and young field survive allocation pressure");
const dense = [149, 151];
dense.named = 157;
dense[symbol] = 163;
function denseRead(receiver) { return receiver[0] + receiver[1]; }
for (let i = 0; i < 6000; i++) denseRead(dense);
check(denseRead(dense) === 300 && dense.named === 157, "present dense slots coexist with expando");
function loadJoin(receiver) { return receiver.join; }
const cleanLookup = [167, 173];
for (let i = 0; i < 6000; i++) loadJoin(cleanLookup);
check(loadJoin(cleanLookup) === Array.prototype.join, "clean inherited lookup");
cleanLookup.join = 179;
check(loadJoin(cleanLookup) === 179 && denseRead(cleanLookup) === 340,
  "expando half blocks inherited shortcut without blocking dense access");
delete cleanLookup.join;
check(loadJoin(cleanLookup) === Array.prototype.join,
  "empty retained expando preserves inherited lookup semantics");
const rareLookup = [181, 191];
Object.setPrototypeOf(rareLookup, {join: 193});
check(loadJoin(rareLookup) === 193 && denseRead(rareLookup) === 372,
  "rare half blocks inherited shortcut without blocking dense access");
function captureTemplate(strings) { return strings; }
function templateSite() {
  return captureTemplate`first\n${1}second${2}third${3}fourth${4}fifth${5}sixth${6}seventh${7}last`;
}
const template = templateSite();
check(templateSite() === template && template.length === 8 && template[0] === "first\n"
  && template[7] === "last" && template.raw[0] === "first\\n", "rooted template strings and cache identity");
const rawDescriptor = Object.getOwnPropertyDescriptor(template, "raw");
check(!rawDescriptor.writable && !rawDescriptor.enumerable && !rawDescriptor.configurable
  && Object.isFrozen(template) && Object.isFrozen(template.raw), "template raw descriptor and shared freeze");
check(Object.keys(template).join() === "0,1,2,3,4,5,6,7"
  && Object.getOwnPropertyNames(template).join() === "0,1,2,3,4,5,6,7,length,raw", "template expando key order");
const invalidCooked = captureTemplate`\unicode`;
check(invalidCooked[0] === undefined && invalidCooked.raw[0] === "\\unicode",
  "invalid cooked template entry retains raw text");
console.log("array-named-expando", checks, inheritedCalls, getterCalls);
