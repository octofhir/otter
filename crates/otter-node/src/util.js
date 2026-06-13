'use strict';
// `node:util` — a practical subset implemented in JS. Focuses on the surface
// the test suite leans on most: inspect (618 uses), format (192), types,
// promisify, inherits, isDeepStrictEqual, deprecate, styleText.
// Run dependency-free through run_builtin_cjs_shim.

const objToString = (v) => Object.prototype.toString.call(v);

// ---------- types ----------
function tagged(tag) {
  return (v) => objToString(v) === `[object ${tag}]`;
}
const typedArrayTags = new Set([
  'Uint8Array', 'Int8Array', 'Uint8ClampedArray', 'Uint16Array', 'Int16Array',
  'Uint32Array', 'Int32Array', 'Float32Array', 'Float64Array',
  'BigInt64Array', 'BigUint64Array',
]);
const types = {
  isDate: tagged('Date'),
  isRegExp: tagged('RegExp'),
  isMap: tagged('Map'),
  isSet: tagged('Set'),
  isWeakMap: tagged('WeakMap'),
  isWeakSet: tagged('WeakSet'),
  isArrayBuffer: tagged('ArrayBuffer'),
  isSharedArrayBuffer: tagged('SharedArrayBuffer'),
  isPromise: tagged('Promise'),
  isGeneratorObject: tagged('Generator'),
  isAsyncFunction: tagged('AsyncFunction'),
  isMapIterator: tagged('Map Iterator'),
  isSetIterator: tagged('Set Iterator'),
  isBoxedPrimitive(v) {
    const t = objToString(v);
    return t === '[object Number]' || t === '[object String]' ||
           t === '[object Boolean]' || t === '[object Symbol]' ||
           t === '[object BigInt]';
  },
  isNativeError(v) { return v instanceof Error; },
  isAnyArrayBuffer(v) {
    return tagged('ArrayBuffer')(v) || tagged('SharedArrayBuffer')(v);
  },
  isTypedArray(v) {
    const t = objToString(v).slice(8, -1);
    return typedArrayTags.has(t);
  },
};
for (const tag of typedArrayTags) {
  types[`is${tag}`] = tagged(tag);
}
types.isUint8Array = tagged('Uint8Array');

// ---------- inspect ----------
const defaultInspectOptions = {
  depth: 2, colors: false, showHidden: false, maxArrayLength: 100,
  breakLength: 128, compact: 3, sorted: false, getters: false,
};

function inspect(value, opts) {
  let options = { ...defaultInspectOptions };
  if (typeof opts === 'boolean') options.showHidden = opts;
  else if (opts && typeof opts === 'object') options = { ...options, ...opts };
  return formatValue(value, options, 0, new Set());
}
inspect.custom = Symbol.for('nodejs.util.inspect.custom');
inspect.defaultOptions = defaultInspectOptions;
inspect.colors = {};
inspect.styles = {};

function quoteString(str) {
  const escaped = str
    .replace(/\\/g, '\\\\')
    .replace(/\n/g, '\\n')
    .replace(/'/g, "\\'");
  return `'${escaped}'`;
}

function formatValue(value, options, depth, seen) {
  if (value === null) return 'null';
  const t = typeof value;
  if (t === 'string') return quoteString(value);
  if (t === 'number') return Object.is(value, -0) ? '-0' : String(value);
  if (t === 'bigint') return `${value}n`;
  if (t === 'boolean' || t === 'undefined') return String(value);
  if (t === 'symbol') return value.toString();
  if (t === 'function') {
    const name = value.name ? `: ${value.name}` : ' (anonymous)';
    const cls = /^class[\s{]/.test(Function.prototype.toString.call(value)) ? 'class' : 'Function';
    return `[${cls}${name}]`;
  }

  // object-like
  if (seen.has(value)) return '[Circular *1]';

  if (types.isRegExp(value)) return value.toString();
  if (types.isDate(value)) return Number.isNaN(value.getTime()) ? 'Invalid Date' : value.toISOString();
  if (value instanceof Error) {
    const stack = value.stack;
    return typeof stack === 'string' ? stack : `${value.name}: ${value.message}`;
  }

  if (depth > options.depth && options.depth !== null) {
    if (Array.isArray(value)) return '[Array]';
    return '[Object]';
  }

  // inspect.custom hook
  const custom = value[inspect.custom];
  if (typeof custom === 'function') {
    const r = custom.call(value, options.depth, options);
    if (typeof r === 'string') return r;
    if (r !== value) return formatValue(r, options, depth, seen);
  }

  seen.add(value);
  let out;
  try {
    if (Array.isArray(value)) out = formatArray(value, options, depth, seen);
    else if (types.isMap(value)) out = formatMap(value, options, depth, seen);
    else if (types.isSet(value)) out = formatSet(value, options, depth, seen);
    else if (types.isTypedArray(value)) out = formatArray(Array.from(value), options, depth, seen, value.constructor.name);
    else out = formatObject(value, options, depth, seen);
  } finally {
    seen.delete(value);
  }
  return out;
}

function keyToString(key) {
  if (typeof key === 'symbol') return `[${key.toString()}]`;
  if (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(key)) return key;
  return quoteString(key);
}

function formatArray(arr, options, depth, seen, prefix) {
  const items = [];
  const limit = Math.min(arr.length, options.maxArrayLength);
  for (let i = 0; i < limit; i++) {
    items.push(formatValue(arr[i], options, depth + 1, seen));
  }
  if (arr.length > limit) items.push(`... ${arr.length - limit} more item${arr.length - limit > 1 ? 's' : ''}`);
  const head = prefix ? `${prefix}(${arr.length}) ` : '';
  if (items.length === 0) return `${head}[]`;
  return `${head}[ ${items.join(', ')} ]`;
}

function formatObject(obj, options, depth, seen) {
  const keys = Object.keys(obj);
  const symbols = Object.getOwnPropertySymbols(obj).filter(
    (s) => Object.getOwnPropertyDescriptor(obj, s).enumerable);
  const parts = [];
  for (const key of keys) {
    parts.push(`${keyToString(key)}: ${formatValue(obj[key], options, depth + 1, seen)}`);
  }
  for (const sym of symbols) {
    parts.push(`${keyToString(sym)}: ${formatValue(obj[sym], options, depth + 1, seen)}`);
  }
  let ctorName = '';
  const proto = Object.getPrototypeOf(obj);
  if (proto === null) ctorName = '[Object: null prototype] ';
  else if (proto.constructor && proto.constructor.name && proto.constructor.name !== 'Object') {
    ctorName = `${proto.constructor.name} `;
  }
  if (parts.length === 0) return `${ctorName}{}`;
  return `${ctorName}{ ${parts.join(', ')} }`;
}

function formatMap(map, options, depth, seen) {
  const parts = [];
  for (const [k, v] of map) {
    parts.push(`${formatValue(k, options, depth + 1, seen)} => ${formatValue(v, options, depth + 1, seen)}`);
  }
  if (parts.length === 0) return `Map(0) {}`;
  return `Map(${map.size}) { ${parts.join(', ')} }`;
}

function formatSet(set, options, depth, seen) {
  const parts = [];
  for (const v of set) parts.push(formatValue(v, options, depth + 1, seen));
  if (parts.length === 0) return `Set(0) {}`;
  return `Set(${set.size}) { ${parts.join(', ')} }`;
}

// ---------- format ----------
function formatWithOptions(inspectOptions, ...args) {
  const first = args[0];
  let str = '';
  let a = 0;
  if (typeof first === 'string') {
    a = 1;
    str = first.replace(/%[sdifjoOc%]/g, (match) => {
      if (match === '%%') return '%';
      if (a >= args.length) return match;
      const arg = args[a];
      switch (match) {
        case '%s': a++; return typeof arg === 'bigint' ? `${arg}n`
          : (typeof arg === 'object' && arg !== null) ? inspect(arg, { ...inspectOptions, depth: 0 })
          : String(arg);
        case '%d': a++; return typeof arg === 'bigint' ? `${arg}n`
          : typeof arg === 'symbol' ? arg.toString() : String(Number(arg));
        case '%i': a++; return typeof arg === 'bigint' ? `${arg}n`
          : typeof arg === 'symbol' ? arg.toString() : String(parseInt(arg, 10));
        case '%f': a++; return typeof arg === 'symbol' ? arg.toString() : String(parseFloat(arg));
        case '%j': a++; try { return JSON.stringify(arg); } catch { return '[Circular]'; }
        case '%o': a++; return inspect(arg, { ...inspectOptions, showHidden: true, depth: 4 });
        case '%O': a++; return inspect(arg, inspectOptions);
        case '%c': a++; return '';
        default: return match;
      }
    });
  }
  for (; a < args.length; a++) {
    const arg = args[a];
    str += (str ? ' ' : '');
    str += (typeof arg === 'string') ? arg : inspect(arg, inspectOptions);
  }
  return str;
}

function format(...args) {
  return formatWithOptions(undefined, ...args);
}

// ---------- isDeepStrictEqual ----------
function isDeepStrictEqual(a, b) {
  return deepEqual(a, b, true, new Map());
}

function deepEqual(a, b, strict, memo) {
  if (Object.is(a, b)) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) {
    return strict ? false : a == b; // eslint-disable-line eqeqeq
  }
  const ta = objToString(a);
  if (ta !== objToString(b)) return false;
  if (memo.get(a) === b) return true;
  memo.set(a, b);

  if (types.isDate(a)) return a.getTime() === b.getTime();
  if (types.isRegExp(a)) return a.source === b.source && a.flags === b.flags;
  if (Array.isArray(a)) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!deepEqual(a[i], b[i], strict, memo)) return false;
    return compareKeys(a, b, strict, memo, true);
  }
  if (types.isMap(a)) {
    if (a.size !== b.size) return false;
    for (const [k, v] of a) { if (!b.has(k) || !deepEqual(v, b.get(k), strict, memo)) return false; }
    return true;
  }
  if (types.isSet(a)) {
    if (a.size !== b.size) return false;
    for (const v of a) if (!b.has(v)) return false;
    return true;
  }
  return compareKeys(a, b, strict, memo, false);
}

function compareKeys(a, b, strict, memo, isArray) {
  const ka = Object.keys(a).filter((k) => !(isArray && /^\d+$/.test(k)));
  const kb = Object.keys(b).filter((k) => !(isArray && /^\d+$/.test(k)));
  if (ka.length !== kb.length) return false;
  for (const k of ka) {
    if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
    if (!deepEqual(a[k], b[k], strict, memo)) return false;
  }
  return true;
}

// ---------- promisify / callbackify ----------
const kCustomPromisify = Symbol.for('nodejs.util.promisify.custom');
function promisify(original) {
  if (typeof original !== 'function') {
    const err = new TypeError('The "original" argument must be of type function.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  if (original[kCustomPromisify]) return original[kCustomPromisify];
  function fn(...args) {
    return new Promise((resolve, reject) => {
      original.call(this, ...args, (err, ...values) => {
        if (err) return reject(err);
        resolve(values[0]);
      });
    });
  }
  Object.setPrototypeOf(fn, Object.getPrototypeOf(original));
  return fn;
}
promisify.custom = kCustomPromisify;

function callbackify(original) {
  if (typeof original !== 'function') {
    const err = new TypeError('The "original" argument must be of type function.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  function fn(...args) {
    const cb = args.pop();
    original.apply(this, args).then(
      (ret) => queueMicrotask(() => cb(null, ret)),
      (err) => queueMicrotask(() => cb(err || new Error('Promise was rejected with a falsy value'))));
  }
  return fn;
}

// ---------- inherits ----------
function inherits(ctor, superCtor) {
  if (ctor === undefined || ctor === null) {
    const err = new TypeError('The "ctor" argument must be of type function.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  if (superCtor === undefined || superCtor === null) {
    const err = new TypeError('The "superCtor" argument must be of type function.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  if (superCtor.prototype === undefined) {
    const err = new TypeError('The "superCtor.prototype" property must be of type object.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  Object.defineProperty(ctor, 'super_', {
    value: superCtor, writable: true, configurable: true,
  });
  Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
}

// ---------- deprecate ----------
function deprecate(fn, msg, code) {
  let warned = false;
  function deprecated(...args) {
    if (!warned) {
      warned = true;
      if (typeof process !== 'undefined' && process.emitWarning) {
        process.emitWarning(msg, 'DeprecationWarning', code);
      }
    }
    return fn.apply(this, args);
  }
  return deprecated;
}

// ---------- ANSI helpers ----------
const ansiPattern = /[][[\]()#;?]*(?:(?:(?:[a-zA-Z\d]*(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?)|(?:(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-nq-uy=><~]))/g;
function stripVTControlCharacters(str) {
  if (typeof str !== 'string') {
    const err = new TypeError('The "str" argument must be of type string.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  return str.replace(ansiPattern, '');
}

const styleCodes = {
  reset: [0, 0], bold: [1, 22], italic: [3, 23], underline: [4, 24],
  red: [31, 39], green: [32, 39], yellow: [33, 39], blue: [34, 39],
  magenta: [35, 39], cyan: [36, 39], white: [37, 39], gray: [90, 39],
};
function styleText(format, text) {
  if (typeof text !== 'string') {
    const err = new TypeError('The "text" argument must be of type string.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  const formats = Array.isArray(format) ? format : [format];
  for (const f of formats) {
    if (!Object.prototype.hasOwnProperty.call(styleCodes, f)) {
      const err = new TypeError(`The value "${f}" is invalid for argument 'format'.`);
      err.code = 'ERR_INVALID_ARG_VALUE';
      throw err;
    }
  }
  // No TTY assumption: return text unstyled (Node also strips when not a TTY).
  return text;
}

// ---------- misc ----------
function debuglog() {
  const fn = () => {};
  fn.enabled = false;
  return fn;
}

const exportsObj = {
  inspect,
  format,
  formatWithOptions,
  types,
  isDeepStrictEqual,
  promisify,
  callbackify,
  inherits,
  deprecate,
  debuglog,
  debug: debuglog,
  stripVTControlCharacters,
  styleText,
  isArray: Array.isArray,
  isError(e) { return e instanceof Error || objToString(e) === '[object Error]'; },
  isFunction(v) { return typeof v === 'function'; },
  isString(v) { return typeof v === 'string'; },
  isNumber(v) { return typeof v === 'number'; },
  isBoolean(v) { return typeof v === 'boolean'; },
  isNull(v) { return v === null; },
  isUndefined(v) { return v === undefined; },
  isNullOrUndefined(v) { return v == null; },
  isObject(v) { return v !== null && typeof v === 'object'; },
  isPrimitive(v) { return v === null || (typeof v !== 'object' && typeof v !== 'function'); },
  isRegExp: types.isRegExp,
  isDate: types.isDate,
  isSymbol(v) { return typeof v === 'symbol'; },
  isBuffer(v) { return false; },
  normalizeEncoding(enc) {
    if (!enc) return 'utf8';
    const e = String(enc).toLowerCase();
    const map = { utf8: 'utf8', 'utf-8': 'utf8', ucs2: 'utf16le', 'ucs-2': 'utf16le',
      utf16le: 'utf16le', 'utf-16le': 'utf16le', latin1: 'latin1', binary: 'latin1',
      base64: 'base64', base64url: 'base64url', hex: 'hex', ascii: 'ascii' };
    return map[e];
  },
  toUSVString(s) { return String(s); },
  getSystemErrorName(err) { return `Unknown system error ${err}`; },
  getCallSites() { return []; },
  getCallSite() { return []; },
  _extend(target, source) {
    if (source === null || typeof source !== 'object') return target;
    for (const k of Object.keys(source)) target[k] = source[k];
    return target;
  },
};

module.exports = exportsObj;
