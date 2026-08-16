'use strict';
// The realm contract Node's own lib files are compiled against: the
// `primordials` bag of uncurried frozen intrinsics and the `internalBinding`
// registry. Vendored files receive both through one injected require of this
// module, which stands in for the parameters Node's native wrapper passes.
//
// `primordials` is a Proxy that derives entries from their names on first
// use — `ArrayPrototypePush` is the uncurried `Array.prototype.push`,
// `NumberIsNaN` is `Number.isNaN`, `SymbolAsyncIterator` is the well-known
// symbol — with an explicit table for the names that do not follow the
// pattern (Safe* containers, %TypedArray%, uncurryThis itself).

const ReflectApply = Reflect.apply;

function uncurryThis(fn) {
  return function (thisArg, ...args) {
    return ReflectApply(fn, thisArg, args);
  };
}

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
  SafePromiseAll: PromiseAll,
  SafePromiseAllReturnVoid: async (v) => { await Promise.all(v); },
  SafePromiseAllReturnArrayLike: PromiseAll,
  SafePromiseAllSettled: (v) => Promise.allSettled(v),
  SafePromiseAllSettledReturnVoid: async (v) => { await Promise.allSettled(v); },
  SafePromiseRace: (v) => Promise.race(v),
  SafePromisePrototypeFinally: (promise, onFinally) => promise.finally(onFinally),
  PromisePrototypeCatch: (promise, onRejected) => promise.catch(onRejected),
  // `queueMicrotask` rides along for files that pull it from primordials.
  queueMicrotask: globalThis.queueMicrotask,
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

const lowerFirst = (name) => name.charAt(0).toLowerCase() + name.slice(1);

const cache = new Map();

function derive(name) {
  if (name in explicit) return explicit[name];
  if (name in symbolWells) return symbolWells[name];
  if (name in constructors) return constructors[name];
  if (name in namespaces) return namespaces[name];

  // `<Base>Prototype` — the prototype object itself.
  let m = /^([A-Z][A-Za-z0-9]*?)Prototype$/.exec(name);
  if (m) {
    const base = constructors[m[1]] ??
      (m[1] === 'TypedArray' ? TypedArray : undefined) ??
      (m[1] === 'AsyncGenerator' ? AsyncGeneratorFunction.prototype.prototype : undefined);
    if (base) return base.prototype ?? base;
  }

  // `<Base>PrototypeGet<Name>` / `<Base>PrototypeSet<Name>` — uncurried
  // accessor halves, tried before the plain method rule so getter names
  // never resolve to `undefined` methods.
  m = /^([A-Z][A-Za-z0-9]*?)Prototype(Get|Set)([A-Z][A-Za-z0-9]*)$/.exec(name);
  if (m) {
    const base = m[1] === 'TypedArray' ? TypedArray : constructors[m[1]];
    const proto = base?.prototype;
    if (proto) {
      const key = m[3] in symbolWells === false && m[3].startsWith('Symbol')
        ? symbolWells[`Symbol${m[3].slice(6)}`]
        : lowerFirst(m[3]);
      const lookup = m[3].startsWith('Symbol')
        ? symbolWells[m[3]] ?? Symbol[lowerFirst(m[3].slice(6))]
        : lowerFirst(m[3]);
      const desc = Object.getOwnPropertyDescriptor(proto, lookup ?? key);
      const half = desc?.[m[2] === 'Get' ? 'get' : 'set'];
      if (half) return uncurryThis(half);
      // Fall through: names like `MapPrototypeGetSize` exist, but so do
      // plain methods that merely start with Get (`MapPrototypeGet`).
    }
  }

  // `<Base>Prototype<Method>` — uncurried prototype method.
  m = /^([A-Z][A-Za-z0-9]*?)Prototype([A-Z][A-Za-z0-9]*)$/.exec(name);
  if (m) {
    const base = m[1] === 'TypedArray' ? TypedArray : constructors[m[1]];
    const proto = base?.prototype;
    if (proto) {
      const key = m[2].startsWith('Symbol')
        ? symbolWells[m[2]] ?? Symbol[lowerFirst(m[2].slice(6))]
        : lowerFirst(m[2]);
      const method = proto[key];
      if (typeof method === 'function') return uncurryThis(method);
      const desc = Object.getOwnPropertyDescriptor(proto, key);
      if (desc?.get) return uncurryThis(desc.get);
    }
  }

  // `<Namespace><Member>` — statics on constructors and namespaces:
  // ArrayIsArray, ObjectDefineProperty, MathMin, JSONStringify, ReflectApply,
  // NumberMAX_SAFE_INTEGER, PromiseResolve.
  m = /^([A-Z][A-Za-z0-9]*?)([A-Z][A-Za-z0-9_]*)$/.exec(name);
  if (m) {
    for (const [prefix, holder] of [
      ...Object.entries(namespaces),
      ...Object.entries(constructors),
    ]) {
      if (!holder || !name.startsWith(prefix)) continue;
      const rest = name.slice(prefix.length);
      if (!/^[A-Z]/.test(rest)) continue;
      const candidates = rest === rest.toUpperCase()
        ? [rest]
        : [lowerFirst(rest), rest];
      for (const key of candidates) {
        if (key in holder) {
          const member = holder[key];
          return typeof member === 'function' ? member.bind(holder) : member;
        }
      }
    }
  }
  return undefined;
}

const primordials = new Proxy(Object.create(null), {
  get(_target, name) {
    if (typeof name !== 'string') return undefined;
    if (cache.has(name)) return cache.get(name);
    const value = derive(name);
    cache.set(name, value);
    return value;
  },
  has(_target, name) {
    return typeof name === 'string' && primordials[name] !== undefined;
  },
  set(_target, name, value) {
    cache.set(name, value);
    return true;
  },
});

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
  ['EMFILE', -24, 'too many open files'],
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
const uvErrmap = new Map(uvErrors.map(([name, code, message]) => [code, [name, message]]));
const uvNameToCode = new Map(uvErrors.map(([name, code]) => [name, code]));

const privateSymbols = {
  arrow_message_private_symbol: Symbol('node:arrowMessage'),
  decorated_private_symbol: Symbol('node:decorated'),
  transfer_mode_private_symbol: Symbol('node:transferMode'),
};

const bindings = {
  uv: {
    errname(code) {
      return uvErrmap.get(code)?.[0] ?? `UNKNOWN`;
    },
    getErrorMap() { return uvErrmap; },
    getErrorMessage(code) { return uvErrmap.get(code)?.[1] ?? 'unknown error'; },
    ...Object.fromEntries(uvErrors.map(([name, code]) => [`UV_${name}`, code])),
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
    fs: {},
    crypto: {},
    zlib: {},
    trace: {},
  },
  util: {
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
    getProxyDetails(_value, _showProxy) { return undefined; },
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
    // ALL_PROPERTIES = 0, ONLY_ENUMERABLE = 2 — inspect passes both.
    getOwnNonIndexProperties(object, filter) {
      const names = Object.getOwnPropertyNames(object)
        .filter((k) => !/^\d+$/.test(k));
      if ((filter & 2) === 0) return names;
      return names.filter((k) =>
        Object.getOwnPropertyDescriptor(object, k)?.enumerable === true);
    },
  },
  config: {
    hasIntl: false,
    hasSmallICU: false,
    isDebugBuild: false,
  },
  buffer: {
    compare(a, b) { return Buffer.compare(a, b); },
  },
  os: {
    getOSInformation() { return ['', '', '']; },
  },
  types: {},
  string_decoder: {},
  messaging: {},
  profiler: {},
};

function internalBinding(name) {
  const binding = bindings[name];
  if (binding === undefined) {
    throw new Error(`internalBinding('${name}') is not provided by this realm`);
  }
  return binding;
}

module.exports = { primordials, internalBinding };
