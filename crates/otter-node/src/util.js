'use strict';
// `node:util` — a practical subset implemented in JS. Focuses on the surface
// the test suite leans on most: inspect (618 uses), format (192), types,
// promisify, inherits, isDeepStrictEqual, deprecate, styleText.
// Run dependency-free through run_builtin_cjs_shim.

const objToString = (v) => Object.prototype.toString.call(v);

// Node's lib/internal/errors invalidArgTypeHelper suffix, used by the
// ERR_INVALID_ARG_TYPE messages this module raises.
function invalidArgTypeSuffix(input) {
  if (input == null) return ` Received ${input}`;
  if (typeof input === 'function') {
    return ` Received function ${input.name}`;
  }
  if (typeof input === 'object') {
    if (input.constructor && input.constructor.name) {
      return ` Received an instance of ${input.constructor.name}`;
    }
    return ` Received an instance of Object`;
  }
  if (typeof input === 'string') return ` Received type string ('${input}')`;
  return ` Received type ${typeof input} (${String(input)})`;
}
function argTypeError(name, expected, input) {
  const e = new TypeError(`The "${name}" ${expected}.${invalidArgTypeSuffix(input)}`);
  e.code = 'ERR_INVALID_ARG_TYPE';
  return e;
}

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

function indentStr(depth) {
  return '  '.repeat(depth);
}

// §reduceToSingleString — join already-formatted entries either onto one line
// (`{ a, b }`) or, when `compact === false` or the single line would exceed
// `breakLength` (or an entry already spans lines), across multiple indented
// lines:
//   prefix{
//     a,
//     b
//   }
function reduceToSingleString(parts, prefix, braces, options, depth) {
  if (parts.length === 0) return `${prefix}${braces[0]}${braces[1]}`;
  const hasNewline = parts.some((p) => p.includes('\n'));
  if (options.compact !== false && !hasNewline) {
    const single = `${prefix}${braces[0]} ${parts.join(', ')} ${braces[1]}`;
    const start = single.length + depth * 2;
    if (start <= options.breakLength) return single;
  }
  const inner = indentStr(depth + 1);
  const body = parts.map((p) => inner + p).join(',\n');
  return `${prefix}${braces[0]}\n${body}\n${indentStr(depth)}${braces[1]}`;
}

function formatArray(arr, options, depth, seen, prefix) {
  const items = [];
  const limit = Math.min(arr.length, options.maxArrayLength);
  for (let i = 0; i < limit; i++) {
    items.push(formatValue(arr[i], options, depth + 1, seen));
  }
  if (arr.length > limit) items.push(`... ${arr.length - limit} more item${arr.length - limit > 1 ? 's' : ''}`);
  const head = prefix ? `${prefix}(${arr.length}) ` : '';
  return reduceToSingleString(items, head, ['[', ']'], options, depth);
}

function objectPrefix(obj) {
  const tag = obj[Symbol.toStringTag];
  const proto = Object.getPrototypeOf(obj);
  if (proto === null) {
    return `[${typeof tag === 'string' ? tag : 'Object'}: null prototype] `;
  }
  let name = '';
  if (proto.constructor && proto.constructor.name && proto.constructor.name !== 'Object') {
    name = proto.constructor.name;
  }
  if (typeof tag === 'string') {
    return name ? `${name} [${tag}] ` : `Object [${tag}] `;
  }
  return name ? `${name} ` : '';
}

function formatObject(obj, options, depth, seen) {
  const parts = [];
  for (const key of Object.keys(obj)) {
    parts.push(`${keyToString(key)}: ${formatValue(obj[key], options, depth + 1, seen)}`);
  }
  for (const sym of Object.getOwnPropertySymbols(obj)) {
    if (Object.getOwnPropertyDescriptor(obj, sym).enumerable) {
      parts.push(`${keyToString(sym)}: ${formatValue(obj[sym], options, depth + 1, seen)}`);
    }
  }
  return reduceToSingleString(parts, objectPrefix(obj), ['{', '}'], options, depth);
}

function formatMap(map, options, depth, seen) {
  const parts = [];
  for (const [k, v] of map) {
    parts.push(`${formatValue(k, options, depth + 1, seen)} => ${formatValue(v, options, depth + 1, seen)}`);
  }
  return reduceToSingleString(parts, `Map(${map.size}) `, ['{', '}'], options, depth);
}

function formatSet(set, options, depth, seen) {
  const parts = [];
  for (const v of set) parts.push(formatValue(v, options, depth + 1, seen));
  return reduceToSingleString(parts, `Set(${set.size}) `, ['{', '}'], options, depth);
}

// ---------- format ----------
// Node groups integer digits with "_" separators (every 3 from the right) for
// numbers and bigints in inspect / format. Non-integer or exponential strings
// (e.g. "1.5", "1.18e+21") are left untouched.
function groupDigits(s) {
  const neg = s[0] === '-';
  const digits = neg ? s.slice(1) : s;
  if (digits.length <= 3 || !/^\d+$/.test(digits)) return s;
  let out = '';
  let count = 0;
  for (let i = digits.length - 1; i >= 0; i--) {
    out = digits[i] + out;
    if (++count % 3 === 0 && i !== 0) out = `_${out}`;
  }
  return (neg ? '-' : '') + out;
}
// Node renders negative zero as "-0" in %d / %i / %f (String() would drop it).
// Thousands separators apply only when `numericSeparator` is enabled (default
// off, per Node).
function numToStr(n, sep) {
  if (Object.is(n, -0)) return '-0';
  const s = String(n);
  return sep ? groupDigits(s) : s;
}
function bigIntToStr(n, sep) {
  const s = String(n);
  return `${sep ? groupDigits(s) : s}n`;
}

// §%s — an object whose `toString` is still a built-in (Object/Array/...) is
// rendered with inspect(depth:0); an object that overrides `toString` is
// stringified through it (so `{ toString() { return 'Foo'; } }` → "Foo").
function hasBuiltInToString(o) {
  let proto = o;
  while (proto !== null) {
    const desc = Object.getOwnPropertyDescriptor(proto, 'toString');
    if (desc) {
      // A non-callable `toString` is not a usable override → inspect.
      if (typeof desc.value !== 'function') return true;
      return proto === Object.prototype || proto === Array.prototype ||
        proto === Error.prototype;
    }
    proto = Object.getPrototypeOf(proto);
  }
  return true;
}

function formatWithOptions(inspectOptions, ...args) {
  const first = args[0];
  const sep = !!(inspectOptions && inspectOptions.numericSeparator);
  let str = '';
  let a = 0;
  if (typeof first === 'string') {
    a = 1;
    str = first.replace(/%[sdifjoOc%]/g, (match) => {
      if (match === '%%') return '%';
      if (a >= args.length) return match;
      const arg = args[a];
      switch (match) {
        case '%s': a++; return typeof arg === 'bigint' ? bigIntToStr(arg, sep)
          : typeof arg === 'number' ? numToStr(arg, sep)
          : (typeof arg === 'object' && arg !== null && hasBuiltInToString(arg))
            ? inspect(arg, { ...inspectOptions, depth: 0 })
            : String(arg);
        case '%d': a++; return typeof arg === 'bigint' ? bigIntToStr(arg, sep)
          : typeof arg === 'symbol' ? 'NaN' : numToStr(Number(arg), sep);
        case '%i': a++; return typeof arg === 'bigint' ? bigIntToStr(arg, sep)
          : typeof arg === 'symbol' ? 'NaN' : numToStr(parseInt(arg, 10), sep);
        case '%f': a++; return typeof arg === 'symbol' ? 'NaN' : numToStr(parseFloat(arg), sep);
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
  // Honor mutations to util.inspect.defaultOptions (e.g. numericSeparator).
  return formatWithOptions(inspect.defaultOptions, ...args);
}

// ---------- isDeepStrictEqual ----------
function isDeepStrictEqual(a, b, skipPrototype) {
  return deepEqual(a, b, true, new Map(), !!skipPrototype);
}

// Detect the TRUE boxed-primitive class of an object (independent of any
// @@toStringTag / prototype tampering) by trying each wrapper's own valueOf,
// which throws on a receiver lacking that internal slot.
function boxedPrimitiveType(v) {
  try { Boolean.prototype.valueOf.call(v); return 'boolean'; } catch { /* not a Boolean */ }
  try { Number.prototype.valueOf.call(v); return 'number'; } catch { /* not a Number */ }
  try { String.prototype.valueOf.call(v); return 'string'; } catch { /* not a String */ }
  try { Symbol.prototype.valueOf.call(v); return 'symbol'; } catch { /* not a Symbol */ }
  try { BigInt.prototype.valueOf.call(v); return 'bigint'; } catch { /* not a BigInt */ }
  return null;
}
function unboxPrimitive(v, kind) {
  switch (kind) {
    case 'boolean': return Boolean.prototype.valueOf.call(v);
    case 'number': return Number.prototype.valueOf.call(v);
    case 'string': return String.prototype.valueOf.call(v);
    case 'symbol': return Symbol.prototype.valueOf.call(v);
    case 'bigint': return BigInt.prototype.valueOf.call(v);
    default: return undefined;
  }
}

function isTypedArray(v) {
  return ArrayBuffer.isView(v) && !(v instanceof DataView);
}

function deepEqual(a, b, strict, memo, skipProto) {
  if (Object.is(a, b)) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) {
    if (!strict) {
      // eslint-disable-next-line eqeqeq
      return a == b;
    }
    return false;
  }
  // §strict mode requires the same [[Prototype]] (a tampered toStringTag or
  // re-parented wrapper must NOT compare equal to the genuine type), unless
  // the caller opted into `skipPrototype`.
  if (strict && !skipProto && Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) {
    return false;
  }
  const ta = objToString(a);
  if (ta !== objToString(b)) return false;

  // Boxed primitives compare by their internal value first, then own keys.
  const boxedA = boxedPrimitiveType(a);
  const boxedB = boxedPrimitiveType(b);
  if (boxedA !== null || boxedB !== null) {
    if (boxedA !== boxedB) return false;
    if (!Object.is(unboxPrimitive(a, boxedA), unboxPrimitive(b, boxedB))) return false;
    // fall through to compare any extra own properties (e.g. wrapper.slow)
  }

  if (memo.get(a) === b) return true;
  memo.set(a, b);

  if (types.isDate(a)) {
    if (a.getTime() !== b.getTime()) return false;
    return compareKeys(a, b, strict, memo, skipProto, false);
  }
  if (types.isRegExp(a)) {
    if (a.source !== b.source || a.flags !== b.flags || a.lastIndex !== b.lastIndex) return false;
    return compareKeys(a, b, strict, memo, skipProto, false);
  }
  // §Errors — name / message / cause are non-enumerable, so they are compared
  // explicitly (own enumerable extras still go through compareKeys below).
  if (a instanceof Error && b instanceof Error) {
    if (a.name !== b.name || a.message !== b.message) return false;
    const aHasCause = 'cause' in a;
    const bHasCause = 'cause' in b;
    if (aHasCause !== bHasCause) return false;
    if (aHasCause && !deepEqual(a.cause, b.cause, strict, memo, skipProto)) return false;
    return compareKeys(a, b, strict, memo, skipProto, false);
  }
  if (isTypedArray(a)) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!Object.is(a[i], b[i])) return false;
    return compareKeys(a, b, strict, memo, skipProto, true);
  }
  if (Array.isArray(a)) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) {
      if (!deepEqual(a[i], b[i], strict, memo, skipProto)) return false;
    }
    return compareKeys(a, b, strict, memo, skipProto, true);
  }
  if (types.isMap(a)) {
    if (a.size !== b.size) return false;
    if (!compareMap(a, b, strict, memo, skipProto)) return false;
    return compareKeys(a, b, strict, memo, skipProto, false);
  }
  if (types.isSet(a)) {
    if (a.size !== b.size) return false;
    if (!compareSet(a, b, strict, memo, skipProto)) return false;
    return compareKeys(a, b, strict, memo, skipProto, false);
  }
  return compareKeys(a, b, strict, memo, skipProto, false);
}

// Own enumerable string + symbol keys (Node compares both); array index
// entries are handled by the element walk, so they are excluded here.
function ownEnumerableKeys(obj, isArray) {
  const keys = Object.keys(obj).filter((k) => !(isArray && /^(0|[1-9]\d*)$/.test(k)));
  for (const sym of Object.getOwnPropertySymbols(obj)) {
    if (Object.prototype.propertyIsEnumerable.call(obj, sym)) keys.push(sym);
  }
  return keys;
}

function compareKeys(a, b, strict, memo, skipProto, isArray) {
  const ka = ownEnumerableKeys(a, isArray);
  const kb = ownEnumerableKeys(b, isArray);
  if (ka.length !== kb.length) return false;
  for (const k of ka) {
    if (!Object.prototype.propertyIsEnumerable.call(b, k)) return false;
    if (!deepEqual(a[k], b[k], strict, memo, skipProto)) return false;
  }
  return true;
}

// §Map deep-key matching — keys compared with SameValueZero match directly;
// object keys with no direct hit are matched structurally against the other
// map's unconsumed entries (Node's CompareMap).
function compareMap(a, b, strict, memo, skipProto) {
  const bEntries = [...b];
  const used = new Array(bEntries.length).fill(false);
  for (const [ka, va] of a) {
    if (b.has(ka) && deepEqual(va, b.get(ka), strict, memo, skipProto)) {
      const idx = bEntries.findIndex(([kb], i) => !used[i] && Object.is(kb, ka));
      if (idx !== -1) { used[idx] = true; continue; }
    }
    let matched = false;
    for (let i = 0; i < bEntries.length; i++) {
      if (used[i]) continue;
      const [kb, vb] = bEntries[i];
      if (deepEqual(ka, kb, strict, memo, skipProto) && deepEqual(va, vb, strict, memo, skipProto)) {
        used[i] = true; matched = true; break;
      }
    }
    if (!matched) return false;
  }
  return true;
}

function compareSet(a, b, strict, memo, skipProto) {
  const bValues = [...b];
  const used = new Array(bValues.length).fill(false);
  for (const va of a) {
    if (b.has(va)) {
      const idx = bValues.findIndex((vb, i) => !used[i] && Object.is(vb, va));
      if (idx !== -1) { used[idx] = true; continue; }
    }
    let matched = false;
    for (let i = 0; i < bValues.length; i++) {
      if (used[i]) continue;
      if (deepEqual(va, bValues[i], strict, memo, skipProto)) { used[i] = true; matched = true; break; }
    }
    if (!matched) return false;
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
    throw argTypeError('ctor', 'argument must be of type function', ctor);
  }
  if (superCtor === undefined || superCtor === null) {
    throw argTypeError('superCtor', 'argument must be of type function', superCtor);
  }
  if (superCtor.prototype === undefined) {
    throw argTypeError(
      'superCtor.prototype',
      'property must be of type object',
      superCtor.prototype
    );
  }
  Object.defineProperty(ctor, 'super_', {
    value: superCtor, writable: true, configurable: true,
  });
  Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
}

// ---------- deprecate ----------
function deprecate(fn, msg, code) {
  if (code !== undefined && typeof code !== 'string') {
    throw argTypeError('code', 'argument must be of type string', code);
  }
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
const ansiPattern = new RegExp(
  '[\\u001B\\u009B][[\\]()#;?]*(?:(?:(?:(?:;[-a-zA-Z\\d/#&.:=?%@~_]+)*' +
    '|[a-zA-Z\\d]+(?:;[-a-zA-Z\\d/#&.:=?%@~_]*)*)?(?:\\u0007|\\u001B\\u005C|\\u009C))' +
    '|(?:(?:\\d{1,4}(?:;\\d{0,4})*)?[\\dA-PR-TZcf-ntqry=><~]))',
  'g'
);
function stripVTControlCharacters(str) {
  if (typeof str !== 'string') {
    throw argTypeError('str', 'argument must be of type string', str);
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
