'use strict';
// The realm contract Node's own lib files are compiled against: the
// `primordials` bag of uncurried frozen intrinsics and the `internalBinding`
// registry. Vendored files receive both through one injected require of this
// module, which stands in for the parameters Node's native wrapper passes.
//
// `primordials` is built once, before any vendored file runs, from the closed
// catalog of names those files read: each entry is derived from its name —
// `ArrayPrototypePush` is the uncurried `Array.prototype.push`, `NumberIsNaN`
// is `Number.isNaN`, `SymbolAsyncIterator` is the well-known symbol — with an
// explicit table for the names that do not follow the pattern (Safe*
// containers, %TypedArray%, uncurryThis itself).

const ReflectApply = Reflect.apply;
// `uncurryThis(fn)(thisArg, ...args)` is `fn.call(thisArg, ...args)`: a
// bound `call`, with no argument list built per call.
const uncurryThis = Function.prototype.bind.bind(Function.prototype.call);

const constructors = {
  Array, ArrayBuffer, BigInt, Boolean, DataView, Date, Error, EvalError,
  FinalizationRegistry, Function, Map, Number, Object, Promise, Proxy,
  RangeError, ReferenceError, RegExp, Set, String, Symbol, SyntaxError,
  TypeError, URIError, WeakMap, WeakRef, WeakSet,
  BigInt64Array, BigUint64Array, Float32Array, Float64Array, Int8Array,
  Int16Array, Int32Array, Uint8Array, Uint8ClampedArray, Uint16Array,
  Uint32Array,
  AggregateError: globalThis.AggregateError,
  SharedArrayBuffer: globalThis.SharedArrayBuffer,
};

const namespaces = { JSON, Math, Reflect, Atomics: globalThis.Atomics };

const TypedArray = Object.getPrototypeOf(Uint8Array);
const AsyncGeneratorFunction = Object.getPrototypeOf(async function* () {}).constructor;
const GeneratorFunction = Object.getPrototypeOf(function* () {}).constructor;
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const AsyncIteratorPrototype = Object.getPrototypeOf(
  Object.getPrototypeOf(async function* () {}.prototype));

function makeSafe(unsafe, safe) {
  // Enough of Node's makeSafe: the subclass with stable methods.
  return safe;
}

class SafeMap extends Map {}
class SafeSet extends Set {}
class SafeWeakMap extends WeakMap {}
class SafeWeakSet extends WeakSet {}
class SafeWeakRef extends WeakRef {}
class SafeFinalizationRegistry extends FinalizationRegistry {}

class SafeArrayIterator {
  constructor(array) { this._iter = array[Symbol.iterator](); }
  next() { return this._iter.next(); }
  [Symbol.iterator]() { return this; }
}
class SafeStringIterator {
  constructor(string) { this._iter = string[Symbol.iterator](); }
  next() { return this._iter.next(); }
  [Symbol.iterator]() { return this; }
}

const PromiseAll = (v) => Promise.all(v);
const explicit = {
  uncurryThis,
  makeSafe,
  globalThis,
  TypedArray,
  TypedArrayPrototype: TypedArray.prototype,
  AsyncGeneratorFunction,
  GeneratorFunction,
  AsyncFunction,
  AsyncIteratorPrototype,
  IteratorPrototype: Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())),
  SafeMap, SafeSet, SafeWeakMap, SafeWeakSet, SafeWeakRef,
  SafeFinalizationRegistry, SafeArrayIterator, SafeStringIterator,
  // Each of these takes an optional mapping function applied to every entry —
  // the caller passes what to do with each item, not a list of promises it
  // already built. Ignoring that hands the caller its own input back.
  SafePromiseAll: (list, mapFn) =>
    Promise.all(mapFn == null ? list : Array.from(list, (v, i) => mapFn(v, i))),
  SafePromiseAllReturnVoid: async (list, mapFn) => {
    await Promise.all(mapFn == null ? list : Array.from(list, (v, i) => mapFn(v, i)));
  },
  SafePromiseAllReturnArrayLike: (list, mapFn) =>
    Promise.all(mapFn == null ? list : Array.from(list, (v, i) => mapFn(v, i))),
  SafePromiseAllSettled: (list, mapFn) =>
    Promise.allSettled(mapFn == null ? list : Array.from(list, (v, i) => mapFn(v, i))),
  SafePromiseAllSettledReturnVoid: async (list, mapFn) => {
    await Promise.allSettled(mapFn == null ? list : Array.from(list, (v, i) => mapFn(v, i)));
  },
  SafePromiseRace: (v) => Promise.race(v),
  SafePromisePrototypeFinally: (promise, onFinally) => promise.finally(onFinally),
  // The match index of `regexp` in `string`, searched from the start
  // whatever `lastIndex` held.
  SafeStringPrototypeSearch: (string, regexp) => {
    regexp.lastIndex = 0;
    const match = RegExp.prototype.exec.call(regexp, string);
    return match ? match.index : -1;
  },
  PromisePrototypeCatch: (promise, onRejected) => promise.catch(onRejected),
  // Pin the pattern's own `Symbol.match`/`replace`/`search`/`split` and its
  // `constructor` to the originals, so a pattern the runtime uses internally
  // keeps behaving as one however `RegExp.prototype` was meddled with.
  hardenRegExp: (pattern) => {
    const proto = RegExp.prototype;
    for (const well of [Symbol.match, Symbol.matchAll, Symbol.replace, Symbol.search, Symbol.split]) {
      const original = proto[well];
      if (typeof original === 'function') {
        Object.defineProperty(pattern, well, { configurable: true, value: original });
      }
    }
    Object.defineProperty(pattern, 'constructor', { configurable: true, value: RegExp });
    for (const name of ['dotAll', 'flags', 'global', 'hasIndices', 'ignoreCase', 'multiline',
      'source', 'sticky', 'unicode']) {
      Object.defineProperty(pattern, name, { configurable: true, value: pattern[name] });
    }
    return pattern;
  },
  // `queueMicrotask` rides along for files that pull it from primordials.
  queueMicrotask: globalThis.queueMicrotask,
  // The global functions Node's realm copies under their own names.
  decodeURI, decodeURIComponent, encodeURI, encodeURIComponent, escape, unescape,
};

// Symbol wells: SymbolIterator, SymbolAsyncIterator, SymbolDispose, ...
// Missing wells (engines without explicit resource management) fall back to
// Node's own registered polyfill names.
const symbolWells = {
  SymbolIterator: Symbol.iterator,
  SymbolAsyncIterator: Symbol.asyncIterator,
  SymbolHasInstance: Symbol.hasInstance,
  SymbolToPrimitive: Symbol.toPrimitive,
  SymbolToStringTag: Symbol.toStringTag,
  SymbolSpecies: Symbol.species,
  SymbolDispose: Symbol.dispose ?? Symbol.for('nodejs.dispose'),
  SymbolAsyncDispose: Symbol.asyncDispose ?? Symbol.for('nodejs.asyncDispose'),
  SymbolFor: Symbol.for,
  SymbolKeyFor: Symbol.keyFor,
};



const __otterPrimordialMissing = {};

function __otterPrimordialBuild(primordials) {
  // Generated from the current in-repo AST catalog.
  let value;
  value=constructors["AggregateError"]; if(value!==undefined)primordials["AggregateError"]=value;
  p1: {
    value=__otterPrimordialObject(constructors["AggregateError"]); if(value!==undefined){primordials["AggregateErrorPrototype"]=value;break p1;}
    value=__otterPrimordialStatic(constructors["AggregateError"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["AggregateErrorPrototype"]=value;break p1;}
  }
  value=constructors["Array"]; if(value!==undefined)primordials["Array"]=value;
  value=constructors["ArrayBuffer"]; if(value!==undefined)primordials["ArrayBuffer"]=value;
  p4: {
    value=__otterPrimordialStatic(constructors["Array"],"bufferIsView","BufferIsView"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayBufferIsView"]=value;break p4;}
    value=__otterPrimordialStatic(constructors["ArrayBuffer"],"isView","IsView"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayBufferIsView"]=value;break p4;}
  }
  p5: {
    value=__otterPrimordialObject(constructors["ArrayBuffer"]); if(value!==undefined){primordials["ArrayBufferPrototype"]=value;break p5;}
    value=__otterPrimordialStatic(constructors["Array"],"bufferPrototype","BufferPrototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayBufferPrototype"]=value;break p5;}
    value=__otterPrimordialStatic(constructors["ArrayBuffer"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayBufferPrototype"]=value;break p5;}
  }
  p6: {
    value=__otterPrimordialHalf(constructors["ArrayBuffer"],"byteLength",null,null,"get"); if(value!==undefined){primordials["ArrayBufferPrototypeGetByteLength"]=value;break p6;}
    value=__otterPrimordialMethod(constructors["ArrayBuffer"],"getByteLength",null,null); if(value!==undefined){primordials["ArrayBufferPrototypeGetByteLength"]=value;break p6;}
    value=__otterPrimordialStatic(constructors["Array"],"bufferPrototypeGetByteLength","BufferPrototypeGetByteLength"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayBufferPrototypeGetByteLength"]=value;break p6;}
    value=__otterPrimordialStatic(constructors["ArrayBuffer"],"prototypeGetByteLength","PrototypeGetByteLength"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayBufferPrototypeGetByteLength"]=value;break p6;}
  }
  p7: {
    value=__otterPrimordialMethod(constructors["ArrayBuffer"],"slice",null,null); if(value!==undefined){primordials["ArrayBufferPrototypeSlice"]=value;break p7;}
    value=__otterPrimordialStatic(constructors["Array"],"bufferPrototypeSlice","BufferPrototypeSlice"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayBufferPrototypeSlice"]=value;break p7;}
    value=__otterPrimordialStatic(constructors["ArrayBuffer"],"prototypeSlice","PrototypeSlice"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayBufferPrototypeSlice"]=value;break p7;}
  }
  p8: {
    value=__otterPrimordialStatic(constructors["Array"],"from","From"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayFrom"]=value;break p8;}
  }
  p9: {
    value=__otterPrimordialStatic(constructors["Array"],"fromAsync","FromAsync"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayFromAsync"]=value;break p9;}
  }
  p10: {
    value=__otterPrimordialStatic(constructors["Array"],"isArray","IsArray"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayIsArray"]=value;break p10;}
  }
  p11: {
    value=__otterPrimordialObject(constructors["Array"]); if(value!==undefined){primordials["ArrayPrototype"]=value;break p11;}
    value=__otterPrimordialStatic(constructors["Array"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototype"]=value;break p11;}
  }
  p12: {
    value=__otterPrimordialMethod(constructors["Array"],"at",null,null); if(value!==undefined){primordials["ArrayPrototypeAt"]=value;break p12;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeAt","PrototypeAt"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeAt"]=value;break p12;}
  }
  p13: {
    value=__otterPrimordialMethod(constructors["Array"],"every",null,null); if(value!==undefined){primordials["ArrayPrototypeEvery"]=value;break p13;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeEvery","PrototypeEvery"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeEvery"]=value;break p13;}
  }
  p14: {
    value=__otterPrimordialMethod(constructors["Array"],"fill",null,null); if(value!==undefined){primordials["ArrayPrototypeFill"]=value;break p14;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeFill","PrototypeFill"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeFill"]=value;break p14;}
  }
  p15: {
    value=__otterPrimordialMethod(constructors["Array"],"filter",null,null); if(value!==undefined){primordials["ArrayPrototypeFilter"]=value;break p15;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeFilter","PrototypeFilter"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeFilter"]=value;break p15;}
  }
  p16: {
    value=__otterPrimordialMethod(constructors["Array"],"find",null,null); if(value!==undefined){primordials["ArrayPrototypeFind"]=value;break p16;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeFind","PrototypeFind"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeFind"]=value;break p16;}
  }
  p17: {
    value=__otterPrimordialMethod(constructors["Array"],"flatMap",null,null); if(value!==undefined){primordials["ArrayPrototypeFlatMap"]=value;break p17;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeFlatMap","PrototypeFlatMap"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeFlatMap"]=value;break p17;}
  }
  p18: {
    value=__otterPrimordialMethod(constructors["Array"],"forEach",null,null); if(value!==undefined){primordials["ArrayPrototypeForEach"]=value;break p18;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeForEach","PrototypeForEach"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeForEach"]=value;break p18;}
  }
  p19: {
    value=__otterPrimordialMethod(constructors["Array"],"includes",null,null); if(value!==undefined){primordials["ArrayPrototypeIncludes"]=value;break p19;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeIncludes","PrototypeIncludes"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeIncludes"]=value;break p19;}
  }
  p20: {
    value=__otterPrimordialMethod(constructors["Array"],"indexOf",null,null); if(value!==undefined){primordials["ArrayPrototypeIndexOf"]=value;break p20;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeIndexOf","PrototypeIndexOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeIndexOf"]=value;break p20;}
  }
  p21: {
    value=__otterPrimordialMethod(constructors["Array"],"join",null,null); if(value!==undefined){primordials["ArrayPrototypeJoin"]=value;break p21;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeJoin","PrototypeJoin"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeJoin"]=value;break p21;}
  }
  p22: {
    value=__otterPrimordialMethod(constructors["Array"],"map",null,null); if(value!==undefined){primordials["ArrayPrototypeMap"]=value;break p22;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeMap","PrototypeMap"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeMap"]=value;break p22;}
  }
  p23: {
    value=__otterPrimordialMethod(constructors["Array"],"pop",null,null); if(value!==undefined){primordials["ArrayPrototypePop"]=value;break p23;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypePop","PrototypePop"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypePop"]=value;break p23;}
  }
  p24: {
    value=__otterPrimordialMethod(constructors["Array"],"push",null,null); if(value!==undefined){primordials["ArrayPrototypePush"]=value;break p24;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypePush","PrototypePush"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypePush"]=value;break p24;}
  }
  p25: {
    value=__otterPrimordialApply(constructors["Array"],"push"); if(value!==undefined){primordials["ArrayPrototypePushApply"]=value;break p25;}
    value=__otterPrimordialStaticApply(constructors["Array"],"prototypePush","PrototypePush"); if(value!==undefined){primordials["ArrayPrototypePushApply"]=value;break p25;}
    value=__otterPrimordialMethod(constructors["Array"],"pushApply",null,null); if(value!==undefined){primordials["ArrayPrototypePushApply"]=value;break p25;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypePushApply","PrototypePushApply"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypePushApply"]=value;break p25;}
  }
  p26: {
    value=__otterPrimordialMethod(constructors["Array"],"reduce",null,null); if(value!==undefined){primordials["ArrayPrototypeReduce"]=value;break p26;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeReduce","PrototypeReduce"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeReduce"]=value;break p26;}
  }
  p27: {
    value=__otterPrimordialMethod(constructors["Array"],"reverse",null,null); if(value!==undefined){primordials["ArrayPrototypeReverse"]=value;break p27;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeReverse","PrototypeReverse"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeReverse"]=value;break p27;}
  }
  p28: {
    value=__otterPrimordialMethod(constructors["Array"],"shift",null,null); if(value!==undefined){primordials["ArrayPrototypeShift"]=value;break p28;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeShift","PrototypeShift"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeShift"]=value;break p28;}
  }
  p29: {
    value=__otterPrimordialMethod(constructors["Array"],"slice",null,null); if(value!==undefined){primordials["ArrayPrototypeSlice"]=value;break p29;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeSlice","PrototypeSlice"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeSlice"]=value;break p29;}
  }
  p30: {
    value=__otterPrimordialMethod(constructors["Array"],"some",null,null); if(value!==undefined){primordials["ArrayPrototypeSome"]=value;break p30;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeSome","PrototypeSome"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeSome"]=value;break p30;}
  }
  p31: {
    value=__otterPrimordialMethod(constructors["Array"],"sort",null,null); if(value!==undefined){primordials["ArrayPrototypeSort"]=value;break p31;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeSort","PrototypeSort"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeSort"]=value;break p31;}
  }
  p32: {
    value=__otterPrimordialMethod(constructors["Array"],"splice",null,null); if(value!==undefined){primordials["ArrayPrototypeSplice"]=value;break p32;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeSplice","PrototypeSplice"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeSplice"]=value;break p32;}
  }
  p33: {
    value=__otterPrimordialMethod(constructors["Array"],"toSorted",null,null); if(value!==undefined){primordials["ArrayPrototypeToSorted"]=value;break p33;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeToSorted","PrototypeToSorted"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeToSorted"]=value;break p33;}
  }
  p34: {
    value=__otterPrimordialMethod(constructors["Array"],"unshift",null,null); if(value!==undefined){primordials["ArrayPrototypeUnshift"]=value;break p34;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeUnshift","PrototypeUnshift"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeUnshift"]=value;break p34;}
  }
  p35: {
    value=__otterPrimordialApply(constructors["Array"],"unshift"); if(value!==undefined){primordials["ArrayPrototypeUnshiftApply"]=value;break p35;}
    value=__otterPrimordialStaticApply(constructors["Array"],"prototypeUnshift","PrototypeUnshift"); if(value!==undefined){primordials["ArrayPrototypeUnshiftApply"]=value;break p35;}
    value=__otterPrimordialMethod(constructors["Array"],"unshiftApply",null,null); if(value!==undefined){primordials["ArrayPrototypeUnshiftApply"]=value;break p35;}
    value=__otterPrimordialStatic(constructors["Array"],"prototypeUnshiftApply","PrototypeUnshiftApply"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ArrayPrototypeUnshiftApply"]=value;break p35;}
  }
  value=explicit["AsyncFunction"]; if(value!==undefined)primordials["AsyncFunction"]=value;
  value=explicit["AsyncGeneratorFunction"]; if(value!==undefined)primordials["AsyncGeneratorFunction"]=value;
  value=explicit["AsyncIteratorPrototype"]; if(value!==undefined)primordials["AsyncIteratorPrototype"]=value;
  value=namespaces["Atomics"]; if(value!==undefined)primordials["Atomics"]=value;
  value=constructors["BigInt"]; if(value!==undefined)primordials["BigInt"]=value;
  value=constructors["BigInt64Array"]; if(value!==undefined)primordials["BigInt64Array"]=value;
  p42: {
    value=__otterPrimordialMethod(constructors["BigInt"],"toString",null,null); if(value!==undefined){primordials["BigIntPrototypeToString"]=value;break p42;}
    value=__otterPrimordialStatic(constructors["BigInt"],"prototypeToString","PrototypeToString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["BigIntPrototypeToString"]=value;break p42;}
  }
  p43: {
    value=__otterPrimordialMethod(constructors["BigInt"],"valueOf",null,null); if(value!==undefined){primordials["BigIntPrototypeValueOf"]=value;break p43;}
    value=__otterPrimordialStatic(constructors["BigInt"],"prototypeValueOf","PrototypeValueOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["BigIntPrototypeValueOf"]=value;break p43;}
  }
  value=constructors["BigUint64Array"]; if(value!==undefined)primordials["BigUint64Array"]=value;
  value=constructors["Boolean"]; if(value!==undefined)primordials["Boolean"]=value;
  p46: {
    value=__otterPrimordialObject(constructors["Boolean"]); if(value!==undefined){primordials["BooleanPrototype"]=value;break p46;}
    value=__otterPrimordialStatic(constructors["Boolean"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["BooleanPrototype"]=value;break p46;}
  }
  p47: {
    value=__otterPrimordialMethod(constructors["Boolean"],"valueOf",null,null); if(value!==undefined){primordials["BooleanPrototypeValueOf"]=value;break p47;}
    value=__otterPrimordialStatic(constructors["Boolean"],"prototypeValueOf","PrototypeValueOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["BooleanPrototypeValueOf"]=value;break p47;}
  }
  value=constructors["DataView"]; if(value!==undefined)primordials["DataView"]=value;
  p49: {
    value=__otterPrimordialObject(constructors["DataView"]); if(value!==undefined){primordials["DataViewPrototype"]=value;break p49;}
    value=__otterPrimordialStatic(constructors["DataView"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DataViewPrototype"]=value;break p49;}
  }
  p50: {
    value=__otterPrimordialHalf(constructors["DataView"],"buffer",null,null,"get"); if(value!==undefined){primordials["DataViewPrototypeGetBuffer"]=value;break p50;}
    value=__otterPrimordialMethod(constructors["DataView"],"getBuffer",null,null); if(value!==undefined){primordials["DataViewPrototypeGetBuffer"]=value;break p50;}
    value=__otterPrimordialStatic(constructors["DataView"],"prototypeGetBuffer","PrototypeGetBuffer"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DataViewPrototypeGetBuffer"]=value;break p50;}
  }
  p51: {
    value=__otterPrimordialHalf(constructors["DataView"],"byteLength",null,null,"get"); if(value!==undefined){primordials["DataViewPrototypeGetByteLength"]=value;break p51;}
    value=__otterPrimordialMethod(constructors["DataView"],"getByteLength",null,null); if(value!==undefined){primordials["DataViewPrototypeGetByteLength"]=value;break p51;}
    value=__otterPrimordialStatic(constructors["DataView"],"prototypeGetByteLength","PrototypeGetByteLength"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DataViewPrototypeGetByteLength"]=value;break p51;}
  }
  p52: {
    value=__otterPrimordialHalf(constructors["DataView"],"byteOffset",null,null,"get"); if(value!==undefined){primordials["DataViewPrototypeGetByteOffset"]=value;break p52;}
    value=__otterPrimordialMethod(constructors["DataView"],"getByteOffset",null,null); if(value!==undefined){primordials["DataViewPrototypeGetByteOffset"]=value;break p52;}
    value=__otterPrimordialStatic(constructors["DataView"],"prototypeGetByteOffset","PrototypeGetByteOffset"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DataViewPrototypeGetByteOffset"]=value;break p52;}
  }
  value=constructors["Date"]; if(value!==undefined)primordials["Date"]=value;
  p54: {
    value=__otterPrimordialStatic(constructors["Date"],"now","Now"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DateNow"]=value;break p54;}
  }
  p55: {
    value=__otterPrimordialObject(constructors["Date"]); if(value!==undefined){primordials["DatePrototype"]=value;break p55;}
    value=__otterPrimordialStatic(constructors["Date"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototype"]=value;break p55;}
  }
  p56: {
    value=__otterPrimordialHalf(constructors["Date"],"date",null,null,"get"); if(value!==undefined){primordials["DatePrototypeGetDate"]=value;break p56;}
    value=__otterPrimordialMethod(constructors["Date"],"getDate",null,null); if(value!==undefined){primordials["DatePrototypeGetDate"]=value;break p56;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeGetDate","PrototypeGetDate"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeGetDate"]=value;break p56;}
  }
  p57: {
    value=__otterPrimordialHalf(constructors["Date"],"fullYear",null,null,"get"); if(value!==undefined){primordials["DatePrototypeGetFullYear"]=value;break p57;}
    value=__otterPrimordialMethod(constructors["Date"],"getFullYear",null,null); if(value!==undefined){primordials["DatePrototypeGetFullYear"]=value;break p57;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeGetFullYear","PrototypeGetFullYear"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeGetFullYear"]=value;break p57;}
  }
  p58: {
    value=__otterPrimordialHalf(constructors["Date"],"hours",null,null,"get"); if(value!==undefined){primordials["DatePrototypeGetHours"]=value;break p58;}
    value=__otterPrimordialMethod(constructors["Date"],"getHours",null,null); if(value!==undefined){primordials["DatePrototypeGetHours"]=value;break p58;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeGetHours","PrototypeGetHours"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeGetHours"]=value;break p58;}
  }
  p59: {
    value=__otterPrimordialHalf(constructors["Date"],"minutes",null,null,"get"); if(value!==undefined){primordials["DatePrototypeGetMinutes"]=value;break p59;}
    value=__otterPrimordialMethod(constructors["Date"],"getMinutes",null,null); if(value!==undefined){primordials["DatePrototypeGetMinutes"]=value;break p59;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeGetMinutes","PrototypeGetMinutes"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeGetMinutes"]=value;break p59;}
  }
  p60: {
    value=__otterPrimordialHalf(constructors["Date"],"month",null,null,"get"); if(value!==undefined){primordials["DatePrototypeGetMonth"]=value;break p60;}
    value=__otterPrimordialMethod(constructors["Date"],"getMonth",null,null); if(value!==undefined){primordials["DatePrototypeGetMonth"]=value;break p60;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeGetMonth","PrototypeGetMonth"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeGetMonth"]=value;break p60;}
  }
  p61: {
    value=__otterPrimordialHalf(constructors["Date"],"seconds",null,null,"get"); if(value!==undefined){primordials["DatePrototypeGetSeconds"]=value;break p61;}
    value=__otterPrimordialMethod(constructors["Date"],"getSeconds",null,null); if(value!==undefined){primordials["DatePrototypeGetSeconds"]=value;break p61;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeGetSeconds","PrototypeGetSeconds"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeGetSeconds"]=value;break p61;}
  }
  p62: {
    value=__otterPrimordialHalf(constructors["Date"],"time",null,null,"get"); if(value!==undefined){primordials["DatePrototypeGetTime"]=value;break p62;}
    value=__otterPrimordialMethod(constructors["Date"],"getTime",null,null); if(value!==undefined){primordials["DatePrototypeGetTime"]=value;break p62;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeGetTime","PrototypeGetTime"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeGetTime"]=value;break p62;}
  }
  p63: {
    value=__otterPrimordialMethod(constructors["Date"],"toISOString",null,null); if(value!==undefined){primordials["DatePrototypeToISOString"]=value;break p63;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeToISOString","PrototypeToISOString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeToISOString"]=value;break p63;}
  }
  p64: {
    value=__otterPrimordialMethod(constructors["Date"],"toLocaleString",null,null); if(value!==undefined){primordials["DatePrototypeToLocaleString"]=value;break p64;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeToLocaleString","PrototypeToLocaleString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeToLocaleString"]=value;break p64;}
  }
  p65: {
    value=__otterPrimordialMethod(constructors["Date"],"toString",null,null); if(value!==undefined){primordials["DatePrototypeToString"]=value;break p65;}
    value=__otterPrimordialStatic(constructors["Date"],"prototypeToString","PrototypeToString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["DatePrototypeToString"]=value;break p65;}
  }
  value=constructors["Error"]; if(value!==undefined)primordials["Error"]=value;
  p67: {
    value=__otterPrimordialStatic(constructors["Error"],"captureStackTrace","CaptureStackTrace"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ErrorCaptureStackTrace"]=value;break p67;}
  }
  p68: {
    value=__otterPrimordialObject(constructors["Error"]); if(value!==undefined){primordials["ErrorPrototype"]=value;break p68;}
    value=__otterPrimordialStatic(constructors["Error"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ErrorPrototype"]=value;break p68;}
  }
  p69: {
    value=__otterPrimordialMethod(constructors["Error"],"toString",null,null); if(value!==undefined){primordials["ErrorPrototypeToString"]=value;break p69;}
    value=__otterPrimordialStatic(constructors["Error"],"prototypeToString","PrototypeToString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ErrorPrototypeToString"]=value;break p69;}
  }
  value=constructors["EvalError"]; if(value!==undefined)primordials["EvalError"]=value;
  value=constructors["FinalizationRegistry"]; if(value!==undefined)primordials["FinalizationRegistry"]=value;
  value=constructors["Float32Array"]; if(value!==undefined)primordials["Float32Array"]=value;
  value=constructors["Float64Array"]; if(value!==undefined)primordials["Float64Array"]=value;
  value=constructors["Function"]; if(value!==undefined)primordials["Function"]=value;
  p75: {
    value=__otterPrimordialObject(constructors["Function"]); if(value!==undefined){primordials["FunctionPrototype"]=value;break p75;}
    value=__otterPrimordialStatic(constructors["Function"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["FunctionPrototype"]=value;break p75;}
  }
  p76: {
    value=__otterPrimordialMethod(constructors["Function"],"bind",null,null); if(value!==undefined){primordials["FunctionPrototypeBind"]=value;break p76;}
    value=__otterPrimordialStatic(constructors["Function"],"prototypeBind","PrototypeBind"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["FunctionPrototypeBind"]=value;break p76;}
  }
  p77: {
    value=__otterPrimordialMethod(constructors["Function"],"call",null,null); if(value!==undefined){primordials["FunctionPrototypeCall"]=value;break p77;}
    value=__otterPrimordialStatic(constructors["Function"],"prototypeCall","PrototypeCall"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["FunctionPrototypeCall"]=value;break p77;}
  }
  p78: {
    value=__otterPrimordialMethod(constructors["Function"],"symbolHasInstance","SymbolHasInstance","hasInstance"); if(value!==undefined){primordials["FunctionPrototypeSymbolHasInstance"]=value;break p78;}
    value=__otterPrimordialStatic(constructors["Function"],"prototypeSymbolHasInstance","PrototypeSymbolHasInstance"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["FunctionPrototypeSymbolHasInstance"]=value;break p78;}
  }
  p79: {
    value=__otterPrimordialMethod(constructors["Function"],"toString",null,null); if(value!==undefined){primordials["FunctionPrototypeToString"]=value;break p79;}
    value=__otterPrimordialStatic(constructors["Function"],"prototypeToString","PrototypeToString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["FunctionPrototypeToString"]=value;break p79;}
  }
  value=explicit["GeneratorFunction"]; if(value!==undefined)primordials["GeneratorFunction"]=value;
  value=constructors["Int16Array"]; if(value!==undefined)primordials["Int16Array"]=value;
  value=constructors["Int32Array"]; if(value!==undefined)primordials["Int32Array"]=value;
  value=constructors["Int8Array"]; if(value!==undefined)primordials["Int8Array"]=value;
  value=explicit["IteratorPrototype"]; if(value!==undefined)primordials["IteratorPrototype"]=value;
  value=namespaces["JSON"]; if(value!==undefined)primordials["JSON"]=value;
  p86: {
    value=__otterPrimordialStatic(namespaces["JSON"],"parse","Parse"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["JSONParse"]=value;break p86;}
  }
  p87: {
    value=__otterPrimordialStatic(namespaces["JSON"],"stringify","Stringify"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["JSONStringify"]=value;break p87;}
  }
  value=constructors["Map"]; if(value!==undefined)primordials["Map"]=value;
  p89: {
    value=__otterPrimordialObject(constructors["Map"]); if(value!==undefined){primordials["MapPrototype"]=value;break p89;}
    value=__otterPrimordialStatic(constructors["Map"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MapPrototype"]=value;break p89;}
  }
  p90: {
    value=__otterPrimordialMethod(constructors["Map"],"entries",null,null); if(value!==undefined){primordials["MapPrototypeEntries"]=value;break p90;}
    value=__otterPrimordialStatic(constructors["Map"],"prototypeEntries","PrototypeEntries"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MapPrototypeEntries"]=value;break p90;}
  }
  p91: {
    value=__otterPrimordialMethod(constructors["Map"],"get",null,null); if(value!==undefined){primordials["MapPrototypeGet"]=value;break p91;}
    value=__otterPrimordialStatic(constructors["Map"],"prototypeGet","PrototypeGet"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MapPrototypeGet"]=value;break p91;}
  }
  p92: {
    value=__otterPrimordialHalf(constructors["Map"],"size",null,null,"get"); if(value!==undefined){primordials["MapPrototypeGetSize"]=value;break p92;}
    value=__otterPrimordialMethod(constructors["Map"],"getSize",null,null); if(value!==undefined){primordials["MapPrototypeGetSize"]=value;break p92;}
    value=__otterPrimordialStatic(constructors["Map"],"prototypeGetSize","PrototypeGetSize"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MapPrototypeGetSize"]=value;break p92;}
  }
  p93: {
    value=__otterPrimordialMethod(constructors["Map"],"values",null,null); if(value!==undefined){primordials["MapPrototypeValues"]=value;break p93;}
    value=__otterPrimordialStatic(constructors["Map"],"prototypeValues","PrototypeValues"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MapPrototypeValues"]=value;break p93;}
  }
  value=namespaces["Math"]; if(value!==undefined)primordials["Math"]=value;
  p95: {
    value=__otterPrimordialStatic(namespaces["Math"],"abs","Abs"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathAbs"]=value;break p95;}
  }
  p96: {
    value=__otterPrimordialStatic(namespaces["Math"],"ceil","Ceil"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathCeil"]=value;break p96;}
  }
  p97: {
    value=__otterPrimordialStatic(namespaces["Math"],"floor","Floor"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathFloor"]=value;break p97;}
  }
  p98: {
    value=__otterPrimordialStatic(namespaces["Math"],"imul","Imul"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathImul"]=value;break p98;}
  }
  p99: {
    value=__otterPrimordialStatic(namespaces["Math"],"max","Max"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathMax"]=value;break p99;}
  }
  p100: {
    value=__otterPrimordialStaticApply(namespaces["Math"],"max","Max"); if(value!==undefined){primordials["MathMaxApply"]=value;break p100;}
    value=__otterPrimordialStatic(namespaces["Math"],"maxApply","MaxApply"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathMaxApply"]=value;break p100;}
  }
  p101: {
    value=__otterPrimordialStatic(namespaces["Math"],"min","Min"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathMin"]=value;break p101;}
  }
  p102: {
    value=__otterPrimordialStatic(namespaces["Math"],"random","Random"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathRandom"]=value;break p102;}
  }
  p103: {
    value=__otterPrimordialStatic(namespaces["Math"],"round","Round"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathRound"]=value;break p103;}
  }
  p104: {
    value=__otterPrimordialStatic(namespaces["Math"],"sqrt","Sqrt"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathSqrt"]=value;break p104;}
  }
  p105: {
    value=__otterPrimordialStatic(namespaces["Math"],"trunc","Trunc"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["MathTrunc"]=value;break p105;}
  }
  value=constructors["Number"]; if(value!==undefined)primordials["Number"]=value;
  p107: {
    value=__otterPrimordialStatic(constructors["Number"],"isFinite","IsFinite"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberIsFinite"]=value;break p107;}
  }
  p108: {
    value=__otterPrimordialStatic(constructors["Number"],"isInteger","IsInteger"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberIsInteger"]=value;break p108;}
  }
  p109: {
    value=__otterPrimordialStatic(constructors["Number"],"isNaN","IsNaN"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberIsNaN"]=value;break p109;}
  }
  p110: {
    value=__otterPrimordialStatic(constructors["Number"],"isSafeInteger","IsSafeInteger"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberIsSafeInteger"]=value;break p110;}
  }
  p111: {
    value=__otterPrimordialStatic(constructors["Number"],"MAX_SAFE_INTEGER",null); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberMAX_SAFE_INTEGER"]=value;break p111;}
  }
  p112: {
    value=__otterPrimordialStatic(constructors["Number"],"MIN_SAFE_INTEGER",null); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberMIN_SAFE_INTEGER"]=value;break p112;}
  }
  p113: {
    value=__otterPrimordialStatic(constructors["Number"],"parseFloat","ParseFloat"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberParseFloat"]=value;break p113;}
  }
  p114: {
    value=__otterPrimordialStatic(constructors["Number"],"parseInt","ParseInt"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberParseInt"]=value;break p114;}
  }
  p115: {
    value=__otterPrimordialObject(constructors["Number"]); if(value!==undefined){primordials["NumberPrototype"]=value;break p115;}
    value=__otterPrimordialStatic(constructors["Number"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberPrototype"]=value;break p115;}
  }
  p116: {
    value=__otterPrimordialMethod(constructors["Number"],"toFixed",null,null); if(value!==undefined){primordials["NumberPrototypeToFixed"]=value;break p116;}
    value=__otterPrimordialStatic(constructors["Number"],"prototypeToFixed","PrototypeToFixed"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberPrototypeToFixed"]=value;break p116;}
  }
  p117: {
    value=__otterPrimordialMethod(constructors["Number"],"toString",null,null); if(value!==undefined){primordials["NumberPrototypeToString"]=value;break p117;}
    value=__otterPrimordialStatic(constructors["Number"],"prototypeToString","PrototypeToString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberPrototypeToString"]=value;break p117;}
  }
  p118: {
    value=__otterPrimordialMethod(constructors["Number"],"valueOf",null,null); if(value!==undefined){primordials["NumberPrototypeValueOf"]=value;break p118;}
    value=__otterPrimordialStatic(constructors["Number"],"prototypeValueOf","PrototypeValueOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["NumberPrototypeValueOf"]=value;break p118;}
  }
  value=constructors["Object"]; if(value!==undefined)primordials["Object"]=value;
  p120: {
    value=__otterPrimordialStatic(constructors["Object"],"assign","Assign"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectAssign"]=value;break p120;}
  }
  p121: {
    value=__otterPrimordialStatic(constructors["Object"],"create","Create"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectCreate"]=value;break p121;}
  }
  p122: {
    value=__otterPrimordialStatic(constructors["Object"],"defineProperties","DefineProperties"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectDefineProperties"]=value;break p122;}
  }
  p123: {
    value=__otterPrimordialStatic(constructors["Object"],"defineProperty","DefineProperty"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectDefineProperty"]=value;break p123;}
  }
  p124: {
    value=__otterPrimordialStatic(constructors["Object"],"entries","Entries"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectEntries"]=value;break p124;}
  }
  p125: {
    value=__otterPrimordialStatic(constructors["Object"],"freeze","Freeze"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectFreeze"]=value;break p125;}
  }
  p126: {
    value=__otterPrimordialStatic(constructors["Object"],"getOwnPropertyDescriptor","GetOwnPropertyDescriptor"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectGetOwnPropertyDescriptor"]=value;break p126;}
  }
  p127: {
    value=__otterPrimordialStatic(constructors["Object"],"getOwnPropertyDescriptors","GetOwnPropertyDescriptors"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectGetOwnPropertyDescriptors"]=value;break p127;}
  }
  p128: {
    value=__otterPrimordialStatic(constructors["Object"],"getOwnPropertyNames","GetOwnPropertyNames"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectGetOwnPropertyNames"]=value;break p128;}
  }
  p129: {
    value=__otterPrimordialStatic(constructors["Object"],"getOwnPropertySymbols","GetOwnPropertySymbols"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectGetOwnPropertySymbols"]=value;break p129;}
  }
  p130: {
    value=__otterPrimordialMethod(constructors["ObjectGet"],"of",null,null); if(value!==undefined){primordials["ObjectGetPrototypeOf"]=value;break p130;}
    value=__otterPrimordialStatic(constructors["Object"],"getPrototypeOf","GetPrototypeOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectGetPrototypeOf"]=value;break p130;}
  }
  p131: {
    value=__otterPrimordialStatic(constructors["Object"],"hasOwn","HasOwn"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectHasOwn"]=value;break p131;}
  }
  p132: {
    value=__otterPrimordialStatic(constructors["Object"],"is","Is"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectIs"]=value;break p132;}
  }
  p133: {
    value=__otterPrimordialStatic(constructors["Object"],"isExtensible","IsExtensible"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectIsExtensible"]=value;break p133;}
  }
  p134: {
    value=__otterPrimordialStatic(constructors["Object"],"keys","Keys"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectKeys"]=value;break p134;}
  }
  p135: {
    value=__otterPrimordialObject(constructors["Object"]); if(value!==undefined){primordials["ObjectPrototype"]=value;break p135;}
    value=__otterPrimordialStatic(constructors["Object"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectPrototype"]=value;break p135;}
  }
  p136: {
    value=__otterPrimordialMethod(constructors["Object"],"hasOwnProperty",null,null); if(value!==undefined){primordials["ObjectPrototypeHasOwnProperty"]=value;break p136;}
    value=__otterPrimordialStatic(constructors["Object"],"prototypeHasOwnProperty","PrototypeHasOwnProperty"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectPrototypeHasOwnProperty"]=value;break p136;}
  }
  p137: {
    value=__otterPrimordialMethod(constructors["Object"],"isPrototypeOf",null,null); if(value!==undefined){primordials["ObjectPrototypeIsPrototypeOf"]=value;break p137;}
    value=__otterPrimordialStatic(constructors["Object"],"prototypeIsPrototypeOf","PrototypeIsPrototypeOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectPrototypeIsPrototypeOf"]=value;break p137;}
  }
  p138: {
    value=__otterPrimordialMethod(constructors["Object"],"propertyIsEnumerable",null,null); if(value!==undefined){primordials["ObjectPrototypePropertyIsEnumerable"]=value;break p138;}
    value=__otterPrimordialStatic(constructors["Object"],"prototypePropertyIsEnumerable","PrototypePropertyIsEnumerable"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectPrototypePropertyIsEnumerable"]=value;break p138;}
  }
  p139: {
    value=__otterPrimordialMethod(constructors["Object"],"toString",null,null); if(value!==undefined){primordials["ObjectPrototypeToString"]=value;break p139;}
    value=__otterPrimordialStatic(constructors["Object"],"prototypeToString","PrototypeToString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectPrototypeToString"]=value;break p139;}
  }
  p140: {
    value=__otterPrimordialStatic(constructors["Object"],"seal","Seal"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectSeal"]=value;break p140;}
  }
  p141: {
    value=__otterPrimordialMethod(constructors["ObjectSet"],"of",null,null); if(value!==undefined){primordials["ObjectSetPrototypeOf"]=value;break p141;}
    value=__otterPrimordialStatic(constructors["Object"],"setPrototypeOf","SetPrototypeOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectSetPrototypeOf"]=value;break p141;}
  }
  p142: {
    value=__otterPrimordialStatic(constructors["Object"],"values","Values"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ObjectValues"]=value;break p142;}
  }
  value=constructors["Promise"]; if(value!==undefined)primordials["Promise"]=value;
  p144: {
    value=__otterPrimordialObject(constructors["Promise"]); if(value!==undefined){primordials["PromisePrototype"]=value;break p144;}
    value=__otterPrimordialStatic(constructors["Promise"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["PromisePrototype"]=value;break p144;}
  }
  value=explicit["PromisePrototypeCatch"]; if(value!==undefined)primordials["PromisePrototypeCatch"]=value;
  p146: {
    value=__otterPrimordialMethod(constructors["Promise"],"then",null,null); if(value!==undefined){primordials["PromisePrototypeThen"]=value;break p146;}
    value=__otterPrimordialStatic(constructors["Promise"],"prototypeThen","PrototypeThen"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["PromisePrototypeThen"]=value;break p146;}
  }
  p147: {
    value=__otterPrimordialStatic(constructors["Promise"],"reject","Reject"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["PromiseReject"]=value;break p147;}
  }
  p148: {
    value=__otterPrimordialStatic(constructors["Promise"],"resolve","Resolve"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["PromiseResolve"]=value;break p148;}
  }
  p149: {
    value=__otterPrimordialStatic(constructors["Promise"],"withResolvers","WithResolvers"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["PromiseWithResolvers"]=value;break p149;}
  }
  value=constructors["Proxy"]; if(value!==undefined)primordials["Proxy"]=value;
  value=constructors["RangeError"]; if(value!==undefined)primordials["RangeError"]=value;
  p152: {
    value=__otterPrimordialObject(constructors["RangeError"]); if(value!==undefined){primordials["RangeErrorPrototype"]=value;break p152;}
    value=__otterPrimordialStatic(constructors["RangeError"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RangeErrorPrototype"]=value;break p152;}
  }
  value=constructors["ReferenceError"]; if(value!==undefined)primordials["ReferenceError"]=value;
  value=namespaces["Reflect"]; if(value!==undefined)primordials["Reflect"]=value;
  p155: {
    value=__otterPrimordialStatic(namespaces["Reflect"],"apply","Apply"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ReflectApply"]=value;break p155;}
  }
  p156: {
    value=__otterPrimordialStatic(namespaces["Reflect"],"construct","Construct"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ReflectConstruct"]=value;break p156;}
  }
  p157: {
    value=__otterPrimordialStatic(namespaces["Reflect"],"defineProperty","DefineProperty"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ReflectDefineProperty"]=value;break p157;}
  }
  p158: {
    value=__otterPrimordialStatic(namespaces["Reflect"],"get","Get"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ReflectGet"]=value;break p158;}
  }
  p159: {
    value=__otterPrimordialStatic(namespaces["Reflect"],"getOwnPropertyDescriptor","GetOwnPropertyDescriptor"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ReflectGetOwnPropertyDescriptor"]=value;break p159;}
  }
  p160: {
    value=__otterPrimordialStatic(namespaces["Reflect"],"ownKeys","OwnKeys"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["ReflectOwnKeys"]=value;break p160;}
  }
  value=constructors["RegExp"]; if(value!==undefined)primordials["RegExp"]=value;
  p162: {
    value=__otterPrimordialObject(constructors["RegExp"]); if(value!==undefined){primordials["RegExpPrototype"]=value;break p162;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototype"]=value;break p162;}
  }
  p163: {
    value=__otterPrimordialMethod(constructors["RegExp"],"exec",null,null); if(value!==undefined){primordials["RegExpPrototypeExec"]=value;break p163;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeExec","PrototypeExec"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeExec"]=value;break p163;}
  }
  p164: {
    value=__otterPrimordialHalf(constructors["RegExp"],"dotAll",null,null,"get"); if(value!==undefined){primordials["RegExpPrototypeGetDotAll"]=value;break p164;}
    value=__otterPrimordialMethod(constructors["RegExp"],"getDotAll",null,null); if(value!==undefined){primordials["RegExpPrototypeGetDotAll"]=value;break p164;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeGetDotAll","PrototypeGetDotAll"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeGetDotAll"]=value;break p164;}
  }
  p165: {
    value=__otterPrimordialHalf(constructors["RegExp"],"global",null,null,"get"); if(value!==undefined){primordials["RegExpPrototypeGetGlobal"]=value;break p165;}
    value=__otterPrimordialMethod(constructors["RegExp"],"getGlobal",null,null); if(value!==undefined){primordials["RegExpPrototypeGetGlobal"]=value;break p165;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeGetGlobal","PrototypeGetGlobal"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeGetGlobal"]=value;break p165;}
  }
  p166: {
    value=__otterPrimordialHalf(constructors["RegExp"],"hasIndices",null,null,"get"); if(value!==undefined){primordials["RegExpPrototypeGetHasIndices"]=value;break p166;}
    value=__otterPrimordialMethod(constructors["RegExp"],"getHasIndices",null,null); if(value!==undefined){primordials["RegExpPrototypeGetHasIndices"]=value;break p166;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeGetHasIndices","PrototypeGetHasIndices"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeGetHasIndices"]=value;break p166;}
  }
  p167: {
    value=__otterPrimordialHalf(constructors["RegExp"],"ignoreCase",null,null,"get"); if(value!==undefined){primordials["RegExpPrototypeGetIgnoreCase"]=value;break p167;}
    value=__otterPrimordialMethod(constructors["RegExp"],"getIgnoreCase",null,null); if(value!==undefined){primordials["RegExpPrototypeGetIgnoreCase"]=value;break p167;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeGetIgnoreCase","PrototypeGetIgnoreCase"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeGetIgnoreCase"]=value;break p167;}
  }
  p168: {
    value=__otterPrimordialHalf(constructors["RegExp"],"multiline",null,null,"get"); if(value!==undefined){primordials["RegExpPrototypeGetMultiline"]=value;break p168;}
    value=__otterPrimordialMethod(constructors["RegExp"],"getMultiline",null,null); if(value!==undefined){primordials["RegExpPrototypeGetMultiline"]=value;break p168;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeGetMultiline","PrototypeGetMultiline"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeGetMultiline"]=value;break p168;}
  }
  p169: {
    value=__otterPrimordialHalf(constructors["RegExp"],"source",null,null,"get"); if(value!==undefined){primordials["RegExpPrototypeGetSource"]=value;break p169;}
    value=__otterPrimordialMethod(constructors["RegExp"],"getSource",null,null); if(value!==undefined){primordials["RegExpPrototypeGetSource"]=value;break p169;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeGetSource","PrototypeGetSource"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeGetSource"]=value;break p169;}
  }
  p170: {
    value=__otterPrimordialHalf(constructors["RegExp"],"sticky",null,null,"get"); if(value!==undefined){primordials["RegExpPrototypeGetSticky"]=value;break p170;}
    value=__otterPrimordialMethod(constructors["RegExp"],"getSticky",null,null); if(value!==undefined){primordials["RegExpPrototypeGetSticky"]=value;break p170;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeGetSticky","PrototypeGetSticky"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeGetSticky"]=value;break p170;}
  }
  p171: {
    value=__otterPrimordialHalf(constructors["RegExp"],"unicode",null,null,"get"); if(value!==undefined){primordials["RegExpPrototypeGetUnicode"]=value;break p171;}
    value=__otterPrimordialMethod(constructors["RegExp"],"getUnicode",null,null); if(value!==undefined){primordials["RegExpPrototypeGetUnicode"]=value;break p171;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeGetUnicode","PrototypeGetUnicode"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeGetUnicode"]=value;break p171;}
  }
  p172: {
    value=__otterPrimordialMethod(constructors["RegExp"],"symbolReplace","SymbolReplace","replace"); if(value!==undefined){primordials["RegExpPrototypeSymbolReplace"]=value;break p172;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeSymbolReplace","PrototypeSymbolReplace"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeSymbolReplace"]=value;break p172;}
  }
  p173: {
    value=__otterPrimordialMethod(constructors["RegExp"],"symbolSplit","SymbolSplit","split"); if(value!==undefined){primordials["RegExpPrototypeSymbolSplit"]=value;break p173;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeSymbolSplit","PrototypeSymbolSplit"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeSymbolSplit"]=value;break p173;}
  }
  p174: {
    value=__otterPrimordialMethod(constructors["RegExp"],"test",null,null); if(value!==undefined){primordials["RegExpPrototypeTest"]=value;break p174;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeTest","PrototypeTest"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeTest"]=value;break p174;}
  }
  p175: {
    value=__otterPrimordialMethod(constructors["RegExp"],"toString",null,null); if(value!==undefined){primordials["RegExpPrototypeToString"]=value;break p175;}
    value=__otterPrimordialStatic(constructors["RegExp"],"prototypeToString","PrototypeToString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["RegExpPrototypeToString"]=value;break p175;}
  }
  value=explicit["SafeArrayIterator"]; if(value!==undefined)primordials["SafeArrayIterator"]=value;
  value=explicit["SafeFinalizationRegistry"]; if(value!==undefined)primordials["SafeFinalizationRegistry"]=value;
  value=explicit["SafeMap"]; if(value!==undefined)primordials["SafeMap"]=value;
  value=explicit["SafePromiseAll"]; if(value!==undefined)primordials["SafePromiseAll"]=value;
  value=explicit["SafePromiseAllReturnArrayLike"]; if(value!==undefined)primordials["SafePromiseAllReturnArrayLike"]=value;
  value=explicit["SafePromiseAllReturnVoid"]; if(value!==undefined)primordials["SafePromiseAllReturnVoid"]=value;
  value=explicit["SafePromiseAllSettled"]; if(value!==undefined)primordials["SafePromiseAllSettled"]=value;
  value=explicit["SafePromiseAllSettledReturnVoid"]; if(value!==undefined)primordials["SafePromiseAllSettledReturnVoid"]=value;
  value=explicit["SafePromisePrototypeFinally"]; if(value!==undefined)primordials["SafePromisePrototypeFinally"]=value;
  value=explicit["SafePromiseRace"]; if(value!==undefined)primordials["SafePromiseRace"]=value;
  value=explicit["SafeSet"]; if(value!==undefined)primordials["SafeSet"]=value;
  value=explicit["SafeStringIterator"]; if(value!==undefined)primordials["SafeStringIterator"]=value;
  value=explicit["SafeStringPrototypeSearch"]; if(value!==undefined)primordials["SafeStringPrototypeSearch"]=value;
  value=explicit["SafeWeakMap"]; if(value!==undefined)primordials["SafeWeakMap"]=value;
  value=explicit["SafeWeakRef"]; if(value!==undefined)primordials["SafeWeakRef"]=value;
  value=explicit["SafeWeakSet"]; if(value!==undefined)primordials["SafeWeakSet"]=value;
  value=constructors["Set"]; if(value!==undefined)primordials["Set"]=value;
  p193: {
    value=__otterPrimordialObject(constructors["Set"]); if(value!==undefined){primordials["SetPrototype"]=value;break p193;}
    value=__otterPrimordialStatic(constructors["Set"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["SetPrototype"]=value;break p193;}
  }
  p194: {
    value=__otterPrimordialHalf(constructors["Set"],"size",null,null,"get"); if(value!==undefined){primordials["SetPrototypeGetSize"]=value;break p194;}
    value=__otterPrimordialMethod(constructors["Set"],"getSize",null,null); if(value!==undefined){primordials["SetPrototypeGetSize"]=value;break p194;}
    value=__otterPrimordialStatic(constructors["Set"],"prototypeGetSize","PrototypeGetSize"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["SetPrototypeGetSize"]=value;break p194;}
  }
  p195: {
    value=__otterPrimordialMethod(constructors["Set"],"union",null,null); if(value!==undefined){primordials["SetPrototypeUnion"]=value;break p195;}
    value=__otterPrimordialStatic(constructors["Set"],"prototypeUnion","PrototypeUnion"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["SetPrototypeUnion"]=value;break p195;}
  }
  p196: {
    value=__otterPrimordialMethod(constructors["Set"],"values",null,null); if(value!==undefined){primordials["SetPrototypeValues"]=value;break p196;}
    value=__otterPrimordialStatic(constructors["Set"],"prototypeValues","PrototypeValues"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["SetPrototypeValues"]=value;break p196;}
  }
  value=constructors["SharedArrayBuffer"]; if(value!==undefined)primordials["SharedArrayBuffer"]=value;
  value=constructors["String"]; if(value!==undefined)primordials["String"]=value;
  p199: {
    value=__otterPrimordialStatic(constructors["String"],"fromCharCode","FromCharCode"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringFromCharCode"]=value;break p199;}
  }
  p200: {
    value=__otterPrimordialObject(constructors["String"]); if(value!==undefined){primordials["StringPrototype"]=value;break p200;}
    value=__otterPrimordialStatic(constructors["String"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototype"]=value;break p200;}
  }
  p201: {
    value=__otterPrimordialMethod(constructors["String"],"charAt",null,null); if(value!==undefined){primordials["StringPrototypeCharAt"]=value;break p201;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeCharAt","PrototypeCharAt"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeCharAt"]=value;break p201;}
  }
  p202: {
    value=__otterPrimordialMethod(constructors["String"],"charCodeAt",null,null); if(value!==undefined){primordials["StringPrototypeCharCodeAt"]=value;break p202;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeCharCodeAt","PrototypeCharCodeAt"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeCharCodeAt"]=value;break p202;}
  }
  p203: {
    value=__otterPrimordialMethod(constructors["String"],"codePointAt",null,null); if(value!==undefined){primordials["StringPrototypeCodePointAt"]=value;break p203;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeCodePointAt","PrototypeCodePointAt"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeCodePointAt"]=value;break p203;}
  }
  p204: {
    value=__otterPrimordialMethod(constructors["String"],"endsWith",null,null); if(value!==undefined){primordials["StringPrototypeEndsWith"]=value;break p204;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeEndsWith","PrototypeEndsWith"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeEndsWith"]=value;break p204;}
  }
  p205: {
    value=__otterPrimordialMethod(constructors["String"],"includes",null,null); if(value!==undefined){primordials["StringPrototypeIncludes"]=value;break p205;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeIncludes","PrototypeIncludes"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeIncludes"]=value;break p205;}
  }
  p206: {
    value=__otterPrimordialMethod(constructors["String"],"indexOf",null,null); if(value!==undefined){primordials["StringPrototypeIndexOf"]=value;break p206;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeIndexOf","PrototypeIndexOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeIndexOf"]=value;break p206;}
  }
  p207: {
    value=__otterPrimordialMethod(constructors["String"],"lastIndexOf",null,null); if(value!==undefined){primordials["StringPrototypeLastIndexOf"]=value;break p207;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeLastIndexOf","PrototypeLastIndexOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeLastIndexOf"]=value;break p207;}
  }
  p208: {
    value=__otterPrimordialMethod(constructors["String"],"localeCompare",null,null); if(value!==undefined){primordials["StringPrototypeLocaleCompare"]=value;break p208;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeLocaleCompare","PrototypeLocaleCompare"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeLocaleCompare"]=value;break p208;}
  }
  p209: {
    value=__otterPrimordialMethod(constructors["String"],"normalize",null,null); if(value!==undefined){primordials["StringPrototypeNormalize"]=value;break p209;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeNormalize","PrototypeNormalize"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeNormalize"]=value;break p209;}
  }
  p210: {
    value=__otterPrimordialMethod(constructors["String"],"padEnd",null,null); if(value!==undefined){primordials["StringPrototypePadEnd"]=value;break p210;}
    value=__otterPrimordialStatic(constructors["String"],"prototypePadEnd","PrototypePadEnd"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypePadEnd"]=value;break p210;}
  }
  p211: {
    value=__otterPrimordialMethod(constructors["String"],"padStart",null,null); if(value!==undefined){primordials["StringPrototypePadStart"]=value;break p211;}
    value=__otterPrimordialStatic(constructors["String"],"prototypePadStart","PrototypePadStart"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypePadStart"]=value;break p211;}
  }
  p212: {
    value=__otterPrimordialMethod(constructors["String"],"repeat",null,null); if(value!==undefined){primordials["StringPrototypeRepeat"]=value;break p212;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeRepeat","PrototypeRepeat"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeRepeat"]=value;break p212;}
  }
  p213: {
    value=__otterPrimordialMethod(constructors["String"],"replace",null,null); if(value!==undefined){primordials["StringPrototypeReplace"]=value;break p213;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeReplace","PrototypeReplace"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeReplace"]=value;break p213;}
  }
  p214: {
    value=__otterPrimordialMethod(constructors["String"],"replaceAll",null,null); if(value!==undefined){primordials["StringPrototypeReplaceAll"]=value;break p214;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeReplaceAll","PrototypeReplaceAll"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeReplaceAll"]=value;break p214;}
  }
  p215: {
    value=__otterPrimordialMethod(constructors["String"],"slice",null,null); if(value!==undefined){primordials["StringPrototypeSlice"]=value;break p215;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeSlice","PrototypeSlice"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeSlice"]=value;break p215;}
  }
  p216: {
    value=__otterPrimordialMethod(constructors["String"],"split",null,null); if(value!==undefined){primordials["StringPrototypeSplit"]=value;break p216;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeSplit","PrototypeSplit"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeSplit"]=value;break p216;}
  }
  p217: {
    value=__otterPrimordialMethod(constructors["String"],"startsWith",null,null); if(value!==undefined){primordials["StringPrototypeStartsWith"]=value;break p217;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeStartsWith","PrototypeStartsWith"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeStartsWith"]=value;break p217;}
  }
  p218: {
    value=__otterPrimordialMethod(constructors["String"],"substring",null,null); if(value!==undefined){primordials["StringPrototypeSubstring"]=value;break p218;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeSubstring","PrototypeSubstring"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeSubstring"]=value;break p218;}
  }
  p219: {
    value=__otterPrimordialMethod(constructors["String"],"toLowerCase",null,null); if(value!==undefined){primordials["StringPrototypeToLowerCase"]=value;break p219;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeToLowerCase","PrototypeToLowerCase"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeToLowerCase"]=value;break p219;}
  }
  p220: {
    value=__otterPrimordialMethod(constructors["String"],"toUpperCase",null,null); if(value!==undefined){primordials["StringPrototypeToUpperCase"]=value;break p220;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeToUpperCase","PrototypeToUpperCase"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeToUpperCase"]=value;break p220;}
  }
  p221: {
    value=__otterPrimordialMethod(constructors["String"],"toWellFormed",null,null); if(value!==undefined){primordials["StringPrototypeToWellFormed"]=value;break p221;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeToWellFormed","PrototypeToWellFormed"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeToWellFormed"]=value;break p221;}
  }
  p222: {
    value=__otterPrimordialMethod(constructors["String"],"trim",null,null); if(value!==undefined){primordials["StringPrototypeTrim"]=value;break p222;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeTrim","PrototypeTrim"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeTrim"]=value;break p222;}
  }
  p223: {
    value=__otterPrimordialMethod(constructors["String"],"valueOf",null,null); if(value!==undefined){primordials["StringPrototypeValueOf"]=value;break p223;}
    value=__otterPrimordialStatic(constructors["String"],"prototypeValueOf","PrototypeValueOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["StringPrototypeValueOf"]=value;break p223;}
  }
  value=constructors["Symbol"]; if(value!==undefined)primordials["Symbol"]=value;
  value=symbolWells["SymbolAsyncDispose"]; if(value!==undefined)primordials["SymbolAsyncDispose"]=value;
  value=symbolWells["SymbolAsyncIterator"]; if(value!==undefined)primordials["SymbolAsyncIterator"]=value;
  value=symbolWells["SymbolDispose"]; if(value!==undefined)primordials["SymbolDispose"]=value;
  value=symbolWells["SymbolFor"]; if(value!==undefined)primordials["SymbolFor"]=value;
  value=symbolWells["SymbolHasInstance"]; if(value!==undefined)primordials["SymbolHasInstance"]=value;
  value=symbolWells["SymbolIterator"]; if(value!==undefined)primordials["SymbolIterator"]=value;
  value=symbolWells["SymbolKeyFor"]; if(value!==undefined)primordials["SymbolKeyFor"]=value;
  p232: {
    value=__otterPrimordialHalf(constructors["Symbol"],"description",null,null,"get"); if(value!==undefined){primordials["SymbolPrototypeGetDescription"]=value;break p232;}
    value=__otterPrimordialMethod(constructors["Symbol"],"getDescription",null,null); if(value!==undefined){primordials["SymbolPrototypeGetDescription"]=value;break p232;}
    value=__otterPrimordialStatic(constructors["Symbol"],"prototypeGetDescription","PrototypeGetDescription"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["SymbolPrototypeGetDescription"]=value;break p232;}
  }
  p233: {
    value=__otterPrimordialMethod(constructors["Symbol"],"toString",null,null); if(value!==undefined){primordials["SymbolPrototypeToString"]=value;break p233;}
    value=__otterPrimordialStatic(constructors["Symbol"],"prototypeToString","PrototypeToString"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["SymbolPrototypeToString"]=value;break p233;}
  }
  p234: {
    value=__otterPrimordialMethod(constructors["Symbol"],"valueOf",null,null); if(value!==undefined){primordials["SymbolPrototypeValueOf"]=value;break p234;}
    value=__otterPrimordialStatic(constructors["Symbol"],"prototypeValueOf","PrototypeValueOf"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["SymbolPrototypeValueOf"]=value;break p234;}
  }
  p235: {
    value=__otterPrimordialStatic(constructors["Symbol"],"replace","Replace"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["SymbolReplace"]=value;break p235;}
  }
  value=symbolWells["SymbolSpecies"]; if(value!==undefined)primordials["SymbolSpecies"]=value;
  p237: {
    value=__otterPrimordialStatic(constructors["Symbol"],"split","Split"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["SymbolSplit"]=value;break p237;}
  }
  value=symbolWells["SymbolToPrimitive"]; if(value!==undefined)primordials["SymbolToPrimitive"]=value;
  value=symbolWells["SymbolToStringTag"]; if(value!==undefined)primordials["SymbolToStringTag"]=value;
  value=constructors["SyntaxError"]; if(value!==undefined)primordials["SyntaxError"]=value;
  value=constructors["TypeError"]; if(value!==undefined)primordials["TypeError"]=value;
  p242: {
    value=__otterPrimordialObject(constructors["TypeError"]); if(value!==undefined){primordials["TypeErrorPrototype"]=value;break p242;}
    value=__otterPrimordialStatic(constructors["TypeError"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["TypeErrorPrototype"]=value;break p242;}
  }
  value=explicit["TypedArray"]; if(value!==undefined)primordials["TypedArray"]=value;
  value=explicit["TypedArrayPrototype"]; if(value!==undefined)primordials["TypedArrayPrototype"]=value;
  p245: {
    value=__otterPrimordialMethod(TypedArray,"at",null,null); if(value!==undefined){primordials["TypedArrayPrototypeAt"]=value;break p245;}
  }
  p246: {
    value=__otterPrimordialMethod(TypedArray,"fill",null,null); if(value!==undefined){primordials["TypedArrayPrototypeFill"]=value;break p246;}
  }
  p247: {
    value=__otterPrimordialHalf(TypedArray,"buffer",null,null,"get"); if(value!==undefined){primordials["TypedArrayPrototypeGetBuffer"]=value;break p247;}
    value=__otterPrimordialMethod(TypedArray,"getBuffer",null,null); if(value!==undefined){primordials["TypedArrayPrototypeGetBuffer"]=value;break p247;}
  }
  p248: {
    value=__otterPrimordialHalf(TypedArray,"byteLength",null,null,"get"); if(value!==undefined){primordials["TypedArrayPrototypeGetByteLength"]=value;break p248;}
    value=__otterPrimordialMethod(TypedArray,"getByteLength",null,null); if(value!==undefined){primordials["TypedArrayPrototypeGetByteLength"]=value;break p248;}
  }
  p249: {
    value=__otterPrimordialHalf(TypedArray,"byteOffset",null,null,"get"); if(value!==undefined){primordials["TypedArrayPrototypeGetByteOffset"]=value;break p249;}
    value=__otterPrimordialMethod(TypedArray,"getByteOffset",null,null); if(value!==undefined){primordials["TypedArrayPrototypeGetByteOffset"]=value;break p249;}
  }
  p250: {
    value=__otterPrimordialHalf(TypedArray,"length",null,null,"get"); if(value!==undefined){primordials["TypedArrayPrototypeGetLength"]=value;break p250;}
    value=__otterPrimordialMethod(TypedArray,"getLength",null,null); if(value!==undefined){primordials["TypedArrayPrototypeGetLength"]=value;break p250;}
  }
  p251: {
    value=__otterPrimordialHalf(TypedArray,"symbolToStringTag","SymbolToStringTag","toStringTag","get"); if(value!==undefined){primordials["TypedArrayPrototypeGetSymbolToStringTag"]=value;break p251;}
    value=__otterPrimordialMethod(TypedArray,"getSymbolToStringTag",null,null); if(value!==undefined){primordials["TypedArrayPrototypeGetSymbolToStringTag"]=value;break p251;}
  }
  p252: {
    value=__otterPrimordialMethod(TypedArray,"includes",null,null); if(value!==undefined){primordials["TypedArrayPrototypeIncludes"]=value;break p252;}
  }
  p253: {
    value=__otterPrimordialMethod(TypedArray,"set",null,null); if(value!==undefined){primordials["TypedArrayPrototypeSet"]=value;break p253;}
  }
  p254: {
    value=__otterPrimordialMethod(TypedArray,"slice",null,null); if(value!==undefined){primordials["TypedArrayPrototypeSlice"]=value;break p254;}
  }
  p255: {
    value=__otterPrimordialMethod(TypedArray,"subarray",null,null); if(value!==undefined){primordials["TypedArrayPrototypeSubarray"]=value;break p255;}
  }
  value=constructors["URIError"]; if(value!==undefined)primordials["URIError"]=value;
  value=constructors["Uint16Array"]; if(value!==undefined)primordials["Uint16Array"]=value;
  value=constructors["Uint32Array"]; if(value!==undefined)primordials["Uint32Array"]=value;
  value=constructors["Uint8Array"]; if(value!==undefined)primordials["Uint8Array"]=value;
  value=constructors["Uint8ClampedArray"]; if(value!==undefined)primordials["Uint8ClampedArray"]=value;
  value=constructors["WeakMap"]; if(value!==undefined)primordials["WeakMap"]=value;
  p262: {
    value=__otterPrimordialObject(constructors["WeakMap"]); if(value!==undefined){primordials["WeakMapPrototype"]=value;break p262;}
    value=__otterPrimordialStatic(constructors["WeakMap"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["WeakMapPrototype"]=value;break p262;}
  }
  value=constructors["WeakRef"]; if(value!==undefined)primordials["WeakRef"]=value;
  value=constructors["WeakSet"]; if(value!==undefined)primordials["WeakSet"]=value;
  p265: {
    value=__otterPrimordialObject(constructors["WeakSet"]); if(value!==undefined){primordials["WeakSetPrototype"]=value;break p265;}
    value=__otterPrimordialStatic(constructors["WeakSet"],"prototype","Prototype"); if(value!==__otterPrimordialMissing){if(value!==undefined)primordials["WeakSetPrototype"]=value;break p265;}
  }
  value=explicit["decodeURI"]; if(value!==undefined)primordials["decodeURI"]=value;
  value=explicit["decodeURIComponent"]; if(value!==undefined)primordials["decodeURIComponent"]=value;
  value=explicit["encodeURI"]; if(value!==undefined)primordials["encodeURI"]=value;
  value=explicit["encodeURIComponent"]; if(value!==undefined)primordials["encodeURIComponent"]=value;
  value=explicit["escape"]; if(value!==undefined)primordials["escape"]=value;
  value=explicit["globalThis"]; if(value!==undefined)primordials["globalThis"]=value;
  value=explicit["hardenRegExp"]; if(value!==undefined)primordials["hardenRegExp"]=value;
  value=explicit["makeSafe"]; if(value!==undefined)primordials["makeSafe"]=value;
  value=explicit["queueMicrotask"]; if(value!==undefined)primordials["queueMicrotask"]=value;
  value=explicit["uncurryThis"]; if(value!==undefined)primordials["uncurryThis"]=value;
  value=explicit["unescape"]; if(value!==undefined)primordials["unescape"]=value;
}

function __otterPrimordialObject(base) {
  if(base)return base.prototype??base;
}

function __otterPrimordialHalf(base,key,symbolName,symbolFallback,half) {
  const proto=base?.prototype;
  if(proto){
    const fallback=symbolName && !(symbolName in symbolWells)?symbolWells[symbolName]:key;
    const lookup=symbolName?(symbolWells[symbolName]??Symbol[symbolFallback]):key;
    const descriptor=Object.getOwnPropertyDescriptor(proto,lookup??fallback);
    const method=descriptor?.[half];
    if(method)return uncurryThis(method);
  }
}

function __otterPrimordialApply(base,key) {
  const method=base?.prototype?.[key];
  if(typeof method==='function')return (thisArg,args)=>ReflectApply(method,thisArg,args);
}

function __otterPrimordialMethod(base,key,symbolName,symbolFallback) {
  const proto=base?.prototype;
  if(proto){
    const lookup=symbolName?(symbolWells[symbolName]??Symbol[symbolFallback]):key;
    const method=proto[lookup];
    if(typeof method==='function')return uncurryThis(method);
    const descriptor=Object.getOwnPropertyDescriptor(proto,lookup);
    if(descriptor?.get)return uncurryThis(descriptor.get);
  }
}

function __otterPrimordialStatic(holder,key0,key1) {
  if(!holder)return __otterPrimordialMissing;
  let key;
  if(key0 in holder)key=key0;
  else if(key1!==null && key1 in holder)key=key1;
  else return __otterPrimordialMissing;
  const member=holder[key];
  return typeof member==='function'?member.bind(holder):member;
}

function __otterPrimordialStaticApply(holder,key0,key1) {
  if(holder){
    const member=key1===null?holder[key0]:(holder[key0]??holder[key1]);
    if(typeof member==='function')return (args)=>ReflectApply(member,holder,args);
  }
}

// Node builds its per-context primordials eagerly, so every entry is the
// intrinsic as it was before user code could replace it.
const primordials = { __proto__: null };
__otterPrimordialBuild(primordials);

// ---------------------------------------------------------------- bindings --

// Narrow stand-ins for the native bindings the vendored files actually
// consult. Every member here exists because a vendored line reads it.
const uvErrors = [
  ['EACCES', -13, 'permission denied'],
  ['EADDRINUSE', -48, 'address already in use'],
  ['EADDRNOTAVAIL', -49, 'address not available'],
  ['EAGAIN', -35, 'resource temporarily unavailable'],
  ['EBADF', -9, 'bad file descriptor'],
  ['ECANCELED', -89, 'operation canceled'],
  ['ECONNABORTED', -53, 'software caused connection abort'],
  ['ECONNREFUSED', -61, 'connection refused'],
  ['ECONNRESET', -54, 'connection reset by peer'],
  ['EEXIST', -17, 'file already exists'],
  ['EINVAL', -22, 'invalid argument'],
  ['EISDIR', -21, 'illegal operation on a directory'],
  ['EHOSTUNREACH', -65, 'host is unreachable'],
  ['EMFILE', -24, 'too many open files'],
  ['EMSGSIZE', -40, 'message too long'],
  ['ENETUNREACH', -51, 'network is unreachable'],
  ['ENOTSOCK', -38, 'socket operation on non-socket'],
  ['ENOTSUP', -45, 'operation not supported'],
  ['ENFILE', -23, 'file table overflow'],
  ['ENOBUFS', -55, 'no buffer space available'],
  ['ENOENT', -2, 'no such file or directory'],
  ['ENOTCONN', -57, 'socket is not connected'],
  ['ENOTDIR', -20, 'not a directory'],
  ['ENOTEMPTY', -66, 'directory not empty'],
  ['ENOTFOUND', -3008, 'domain name not found'],
  ['EOF', -4095, 'end of file'],
  ['EPERM', -1, 'operation not permitted'],
  ['EPIPE', -32, 'broken pipe'],
  ['ETIMEDOUT', -60, 'connection timed out'],
];
// Built without the iteration protocol: this module loads lazily, after user
// code may have replaced Array or Map iteration.
const uvErrmap = new Map();
const uvConstants = {};
for (let i = 0; i < uvErrors.length; i++) {
  const { 0: name, 1: code, 2: message } = uvErrors[i];
  uvErrmap.set(code, [name, message]);
  uvConstants[`UV_${name}`] = code;
}

const privateSymbols = {
  arrow_message_private_symbol: Symbol('node:arrowMessage'),
  decorated_private_symbol: Symbol('node:decorated'),
  transfer_mode_private_symbol: Symbol('node:transferMode'),
};

const bindings = {
  // `internalBinding('process_methods')` — where Node hangs the members of
  // `process` its C++ owns. This runtime builds `process` carrying them, so
  // the patch has nothing left to install.
  process_methods: {
    patchProcessObject() {},
  },
  // `internalBinding('performance')` — the milestones bootstrap stamps on the
  // timeline `perf_hooks` reports. The module is asked for only when a
  // milestone is reached, so reaching one never drags it into a snapshot.
  performance: {
    markBootstrapComplete() {
      const { performance } = require('perf_hooks');
      performance.nodeTiming.bootstrapComplete = performance.now();
    },
  },
  // `internalBinding('errors')` — the exit codes Node's own C++ names, as
  // the JavaScript that reads them expects to find them.
  errors: {
    // The source location an error's captured stack points at. Lives on a
    // native module because it reads the structured frame snapshot: the
    // caller builds this message before anything has touched `stack`.
    getErrorSourcePositions(error) {
      return require('internal/otter/error_source').getErrorSourcePositions(error);
    },
    exitCodes: {
      kNoFailure: 0,
      kUncaughtException: 1,
      kGenericUserError: 1,
      kInternalJSParseError: 3,
      kInternalJSEvaluationFailure: 4,
      kV8FatalError: 5,
      kBootstrapFailure: 6,
      kExceptionInFatalExceptionHandler: 7,
      kInvalidCommandLineArgument: 9,
      kInvalidFatalExceptionMonkeyPatching: 10,
      kOutOfMemory: 134,
    },
  },
  uv: {
    errname(code) {
      return uvErrmap.get(code)?.[0] ?? `UNKNOWN`;
    },
    getErrorMap() { return uvErrmap; },
    getErrorMessage(code) { return uvErrmap.get(code)?.[1] ?? 'unknown error'; },
    ...uvConstants,
  },
  constants: {
    os: {
      signals: {
        SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6,
        SIGBUS: 10, SIGFPE: 8, SIGKILL: 9, SIGUSR1: 30, SIGSEGV: 11,
        SIGUSR2: 31, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15, SIGCHLD: 20,
        SIGCONT: 19, SIGSTOP: 17, SIGTSTP: 18, SIGTTIN: 21, SIGTTOU: 22,
        SIGURG: 16, SIGXCPU: 24, SIGXFSZ: 25, SIGVTALRM: 26, SIGPROF: 27,
        SIGWINCH: 28, SIGIO: 23, SIGSYS: 12,
      },
      errno: {},
      dlopen: {},
      priority: {},
    },
    // POSIX file constants, as this platform defines them. `fs.js` maps
    // its string flags onto the `O_*` values and reads the file type out
    // of a mode with `S_IF*`.
    fs: {
      // `fs.constants` is handed out as-is, and Node's has no prototype.
      __proto__: null,
      UV_FS_SYMLINK_DIR: 1, UV_FS_SYMLINK_JUNCTION: 2,
      O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2,
      UV_DIRENT_UNKNOWN: 0, UV_DIRENT_FILE: 1, UV_DIRENT_DIR: 2,
      UV_DIRENT_LINK: 3, UV_DIRENT_FIFO: 4, UV_DIRENT_SOCKET: 5,
      UV_DIRENT_CHAR: 6, UV_DIRENT_BLOCK: 7,
      S_IFMT: 0o170000, S_IFREG: 0o100000, S_IFDIR: 0o040000,
      S_IFCHR: 0o020000, S_IFBLK: 0o060000, S_IFIFO: 0o010000,
      S_IFLNK: 0o120000, S_IFSOCK: 0o140000,
      O_CREAT: 0x0200, O_EXCL: 0x0800, O_NOCTTY: 0x20000, O_TRUNC: 0x0400,
      O_APPEND: 0x0008, O_DIRECTORY: 0x100000, O_NOFOLLOW: 0x0100,
      O_SYNC: 0x0080, O_DSYNC: 0x400000, O_SYMLINK: 0x200000,
      O_NONBLOCK: 0x0004,
      S_IRWXU: 0o700, S_IRUSR: 0o400, S_IWUSR: 0o200, S_IXUSR: 0o100,
      S_IRWXG: 0o070, S_IRGRP: 0o040, S_IWGRP: 0o020, S_IXGRP: 0o010,
      S_IRWXO: 0o007, S_IROTH: 0o004, S_IWOTH: 0o002, S_IXOTH: 0o001,
      F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1,
      UV_FS_COPYFILE_EXCL: 1, COPYFILE_EXCL: 1,
      UV_FS_COPYFILE_FICLONE: 2, COPYFILE_FICLONE: 2,
      UV_FS_COPYFILE_FICLONE_FORCE: 4, COPYFILE_FICLONE_FORCE: 4,
    },
    crypto: {},
    // zlib's own constants, plus the stream modes and the flush values
    // Node's binding exports under the same name.
    zlib: {
      Z_NO_FLUSH: 0, Z_PARTIAL_FLUSH: 1, Z_SYNC_FLUSH: 2, Z_FULL_FLUSH: 3,
      Z_FINISH: 4, Z_BLOCK: 5, Z_TREES: 6,
      Z_OK: 0, Z_STREAM_END: 1, Z_NEED_DICT: 2, Z_ERRNO: -1, Z_STREAM_ERROR: -2,
      Z_DATA_ERROR: -3, Z_MEM_ERROR: -4, Z_BUF_ERROR: -5, Z_VERSION_ERROR: -6,
      Z_NO_COMPRESSION: 0, Z_BEST_SPEED: 1, Z_BEST_COMPRESSION: 9,
      Z_DEFAULT_COMPRESSION: -1,
      Z_FILTERED: 1, Z_HUFFMAN_ONLY: 2, Z_RLE: 3, Z_FIXED: 4,
      Z_DEFAULT_STRATEGY: 0,
      ZLIB_VERNUM: 0x12f0,
      DEFLATE: 1, INFLATE: 2, GZIP: 3, GUNZIP: 4, DEFLATERAW: 5, INFLATERAW: 6,
      UNZIP: 7, BROTLI_DECODE: 8, BROTLI_ENCODE: 9,
      ZSTD_COMPRESS: 10, ZSTD_DECOMPRESS: 11,
      Z_MIN_WINDOWBITS: 8, Z_MAX_WINDOWBITS: 15, Z_DEFAULT_WINDOWBITS: 15,
      Z_MIN_CHUNK: 64, Z_MAX_CHUNK: Infinity, Z_DEFAULT_CHUNK: 16 * 1024,
      Z_MIN_MEMLEVEL: 1, Z_MAX_MEMLEVEL: 9, Z_DEFAULT_MEMLEVEL: 8,
      Z_MIN_LEVEL: -1, Z_MAX_LEVEL: 9, Z_DEFAULT_LEVEL: -1,
      BROTLI_OPERATION_PROCESS: 0, BROTLI_OPERATION_FLUSH: 1,
      BROTLI_OPERATION_FINISH: 2, BROTLI_OPERATION_EMIT_METADATA: 3,
      BROTLI_PARAM_MODE: 0, BROTLI_MODE_GENERIC: 0, BROTLI_MODE_TEXT: 1,
      BROTLI_MODE_FONT: 2, BROTLI_DEFAULT_MODE: 0,
      BROTLI_PARAM_QUALITY: 1, BROTLI_MIN_QUALITY: 0, BROTLI_MAX_QUALITY: 11,
      BROTLI_DEFAULT_QUALITY: 11,
      BROTLI_PARAM_LGWIN: 2, BROTLI_MIN_WINDOW_BITS: 10,
      BROTLI_MAX_WINDOW_BITS: 24, BROTLI_LARGE_MAX_WINDOW_BITS: 30,
      BROTLI_DEFAULT_WINDOW: 22,
      BROTLI_PARAM_LGBLOCK: 3, BROTLI_MIN_INPUT_BLOCK_BITS: 16,
      BROTLI_MAX_INPUT_BLOCK_BITS: 24,
      BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING: 4,
      BROTLI_PARAM_SIZE_HINT: 5, BROTLI_PARAM_LARGE_WINDOW: 6,
      BROTLI_PARAM_NPOSTFIX: 7, BROTLI_PARAM_NDIRECT: 8,
      BROTLI_DECODER_RESULT_ERROR: 0, BROTLI_DECODER_RESULT_SUCCESS: 1,
      BROTLI_DECODER_RESULT_NEEDS_MORE_INPUT: 2,
      BROTLI_DECODER_RESULT_NEEDS_MORE_OUTPUT: 3,
      BROTLI_DECODER_PARAM_DISABLE_RING_BUFFER_REALLOCATION: 0,
      BROTLI_DECODER_PARAM_LARGE_WINDOW: 1,
      BROTLI_DECODER_NO_ERROR: 0, BROTLI_DECODER_SUCCESS: 1,
      BROTLI_DECODER_NEEDS_MORE_INPUT: 2, BROTLI_DECODER_NEEDS_MORE_OUTPUT: 3,
      ZSTD_e_continue: 0, ZSTD_e_flush: 1, ZSTD_e_end: 2,
      ZSTD_c_compressionLevel: 100, ZSTD_c_checksumFlag: 201,
      ZSTD_d_windowLogMax: 100,
      ZSTD_CLEVEL_DEFAULT: 3, ZSTD_error_no_error: 0,
    },
    trace: {},
  },
  util: {
    // Where the caller of the function that asked was written: `[line, column,
    // file]`. The test runner stamps it on every test and hook so a report can
    // point at the declaration rather than at the runner.
    getCallerLocation() {
      const stack = new Error().stack ?? '';
      const frames = stack.split('\n').slice(1);
      // 0 is this function, 1 is whoever called it; 2 is that one's caller,
      // which is the site being asked about.
      const frame = frames[2] ?? frames[frames.length - 1] ?? '';
      const match = /\(?([^()]+):(\d+):(\d+)\)?\s*$/.exec(frame.trim());
      if (match === null) return [0, 0, ''];
      return [Number(match[2]), Number(match[3]), match[1]];
    },
    privateSymbols,
    ...privateSymbols,
    constants: {
      kPending: 0,
      kFulfilled: 1,
      kRejected: 2,
      ALL_PROPERTIES: 0,
      ONLY_WRITABLE: 1,
      ONLY_ENUMERABLE: 2,
      ONLY_CONFIGURABLE: 4,
      SKIP_STRINGS: 8,
      SKIP_SYMBOLS: 16,
    },
    // Promise-state constants and probes, the inspect contract.
    kPending: 0,
    kFulfilled: 1,
    kRejected: 2,
    getPromiseDetails(_promise) {
      // Settlement state is not observable from JavaScript; pending is the
      // honest answer until the engine exposes a probe.
      return [0];
    },
    getProxyDetails(value, showProxy = true) {
      const details = require('internal/otter/natives').proxyDetails(value);
      if (details === undefined) return undefined;
      return showProxy ? details : details[0];
    },
    previewEntries(_value) { return [[], false]; },
    getConstructorName(object) {
      let current = object;
      while (current !== null && current !== undefined) {
        const ctor = Object.getOwnPropertyDescriptor(current, 'constructor')?.value;
        if (typeof ctor === 'function' && ctor.name !== '') return ctor.name;
        current = Object.getPrototypeOf(current);
      }
      return Object.prototype.toString.call(object).slice(8, -1);
    },
    getExternalValue(_value) { return 0n; },
    markPromiseAsHandled(promise) {
      // The engine's rejection tracker only reports promises with no
      // reactions; attaching a no-op catch marks it handled.
      promise?.catch?.(() => {});
    },
    detachArrayBuffer(buffer) {
      if (typeof structuredClone === 'function') {
        structuredClone(buffer, { transfer: [buffer] });
      }
    },
    // PropertyFilter bits: ALL_PROPERTIES=0, ONLY_ENUMERABLE=2,
    // SKIP_STRINGS=8, SKIP_SYMBOLS=16. Symbols are part of the answer
    // unless skipped — strict deep-equal counts symbol-keyed expandos.
    getOwnNonIndexProperties(object, filter) {
      const onlyEnumerable = (filter & 2) !== 0;
      const keep = (key) => !onlyEnumerable ||
        Object.getOwnPropertyDescriptor(object, key)?.enumerable === true;
      const out = [];
      if ((filter & 8) === 0) {
        // A byte view answers directly: walking its own names would spell
        // out one string per element before discarding every one of them.
        const direct = require('internal/otter/natives').ownNonIndexKeys(object);
        for (const k of direct ?? Object.getOwnPropertyNames(object)) {
          if (direct === undefined && /^(?:0|[1-9]\d*)$/.test(k)) continue;
          if (keep(k)) out.push(k);
        }
      }
      if ((filter & 16) === 0) {
        for (const s of Object.getOwnPropertySymbols(object)) {
          if (keep(s)) out.push(s);
        }
      }
      return out;
    },
    constructSharedArrayBuffer(byteLength) {
      return typeof SharedArrayBuffer === 'function'
        ? new SharedArrayBuffer(byteLength)
        : undefined;
    },
    guessHandleType(fd) {
      return require('internal/otter/net').guessHandleType(fd);
    },
    defineLazyProperties(target, moduleName, names) {
      for (const name of names) {
        Object.defineProperty(target, name, {
          configurable: true,
          enumerable: true,
          get() {
            const value = require(moduleName)[name];
            Object.defineProperty(target, name, {
              value, writable: true, configurable: true, enumerable: true,
            });
            return value;
          },
          set(value) {
            Object.defineProperty(target, name, {
              value, writable: true, configurable: true, enumerable: true,
            });
          },
        });
      }
    },
    sleep(msec) {
      if (typeof SharedArrayBuffer === 'function' && typeof Atomics === 'object') {
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, msec);
      }
    },
    getCallSites(frameCount) {
      const natives = require('internal/otter/natives');
      // Skip this wrapper and the vendored util.getCallSites frame so the
      // first reported site is their caller.
      return JSON.parse(natives.captureCallSites(2, frameCount));
    },
    // Node's dotenv semantics: a quoted value runs to the matching close
    // quote (across newlines), the rest of that line is discarded, and
    // double quotes expand \n; an unquoted value ends at the line and
    // loses its trailing #-comment.
    parseEnv(content) {
      const out = { __proto__: null };
      const src = String(content);
      const n = src.length;
      let i = 0;
      while (i < n) {
        while (i < n && ' \t\r\n'.includes(src[i])) i++;
        if (i >= n) break;
        let lineEnd = src.indexOf('\n', i);
        if (lineEnd === -1) lineEnd = n;
        if (src[i] === '#') { i = lineEnd + 1; continue; }
        const eq = src.indexOf('=', i);
        if (eq === -1 || eq > lineEnd) { i = lineEnd + 1; continue; }
        let key = src.slice(i, eq).trim();
        if (key.startsWith('export ')) key = key.slice(7).trim();
        let j = eq + 1;
        while (j < n && (src[j] === ' ' || src[j] === '\t')) j++;
        const quote = src[j];
        if (quote === '"' || quote === "'" || quote === '`') {
          const close = src.indexOf(quote, j + 1);
          if (close !== -1) {
            let value = src.slice(j + 1, close);
            if (quote === '"') {
              value = value.replace(/\\n/g, '\n').replace(/\\r/g, '\r');
            }
            if (key !== '') out[key] = value;
            i = src.indexOf('\n', close);
            i = i === -1 ? n : i + 1;
            continue;
          }
        }
        let value = src.slice(j, lineEnd).trim();
        const hash = value.indexOf('#');
        if (hash !== -1) value = value.slice(0, hash).trim();
        if (key !== '') out[key] = value;
        i = lineEnd + 1;
      }
      return out;
    },
  },
  config: {
    hasIntl: false,
    hasSmallICU: false,
    isDebugBuild: false,
  },
  buffer: {
    // Raw byte comparison over any TypedArray view, as the native binding
    // does — Buffer.compare's public argument check refuses Int8Array etc.
    compare(a, b) {
      const ua = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
      const ub = new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
      const len = Math.min(ua.length, ub.length);
      for (let i = 0; i < len; i++) {
        if (ua[i] !== ub[i]) return ua[i] < ub[i] ? -1 : 1;
      }
      if (ua.length === ub.length) return 0;
      return ua.length < ub.length ? -1 : 1;
    },
  },
  os: {
    getOSInformation() { return ['', '', '']; },
  },
  get timers() {
    return require('internal/otter/timers_binding');
  },
  types: {
    isNativeError: (value) => Error.isError(value),
    isPromise: (value) => value instanceof Promise,
  },
  get string_decoder() {
    return require('internal/string_decoder_binding');
  },
  messaging: {},
  profiler: {},
  // The parser binding is its own compat module; a getter defers the require
  // until first use so realm bootstrap stays cycle-free.
  get http_parser() {
    return require('internal/http_parser');
  },
  trace_events: {
    getCategoryEnabledBuffer() { return new Uint8Array(1); },
    trace() {},
  },
  get stream_wrap() {
    return require('internal/otter/stream_wrap');
  },
  get tcp_wrap() {
    return require('internal/otter/tcp_wrap');
  },
  get pipe_wrap() {
    return require('internal/otter/pipe_wrap');
  },
  get udp_wrap() {
    return require('internal/otter/udp_wrap');
  },
  get zlib() {
    return require('internal/otter/zlib_handle');
  },
  get cares_wrap() {
    return require('internal/otter/cares_wrap');
  },
  get fs() {
    return require('internal/otter/fs_binding');
  },
  get fs_dir() {
    return require('internal/otter/fs_dir');
  },
  get fs_event_wrap() {
    return require('internal/otter/fs_event_wrap');
  },
  fs_legacy: {
    // The single fs surface the vendored net stack reads: synchronous
    // fd writes for `makeSyncWrite`. Errors report through the ctx
    // object the way the native binding does.
    writeBuffer(fd, buffer, offset, length, position, _flags, ctx) {
      try {
        require('fs').writeSync(fd, buffer, offset, length, position ?? null);
      } catch (error) {
        if (ctx !== undefined && ctx !== null) {
          ctx.errno = error.errno ?? -1;
          ctx.code = error.code ?? 'EIO';
          ctx.syscall = 'write';
        }
      }
    },
  },
};

function internalBinding(name) {
  const binding = bindings[name];
  if (binding === undefined) {
    throw new Error(`internalBinding('${name}') is not provided by this realm`);
  }
  return binding;
}

module.exports = { primordials, internalBinding };
