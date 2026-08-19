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

  // `<Base>Prototype<Method>Apply` — the spread-call form:
  // ArrayPrototypePushApply(target, items) = Array.prototype.push.apply.
  m = /^([A-Z][A-Za-z0-9]*?)Prototype([A-Z][A-Za-z0-9]*)Apply$/.exec(name);
  if (m) {
    const base = m[1] === 'TypedArray' ? TypedArray : constructors[m[1]];
    const method = base?.prototype?.[lowerFirst(m[2])];
    if (typeof method === 'function') {
      return (thisArg, args) => ReflectApply(method, thisArg, args);
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
        for (const k of Object.getOwnPropertyNames(object)) {
          if (/^(?:0|[1-9]\d*)$/.test(k)) continue;
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
    guessHandleType() { return 'PIPE'; },
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
