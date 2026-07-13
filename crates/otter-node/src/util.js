'use strict';
// `node:util` — a practical subset implemented in JS. Focuses on the surface
// the test suite leans on most: inspect (618 uses), format (192), types,
// promisify, inherits, isDeepStrictEqual, deprecate, styleText.
// Run dependency-free through run_builtin_cjs_shim.

// Native call-site capture (Rust): returns a JSON array of call-site
// records. `getCallSites` skips its own frame and parses the result.
const __captureCallSites = require('__otter_callsites');

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
  isNativeError(v) {
    return typeof Error.isError === 'function' ? Error.isError(v) :
      objToString(v) === '[object Error]';
  },
  isAnyArrayBuffer(v) {
    return tagged('ArrayBuffer')(v) || tagged('SharedArrayBuffer')(v);
  },
  isTypedArray(v) {
    return ArrayBuffer.isView(v) && !(v instanceof DataView);
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

function formatString(str, options) {
  if (options.compact === false && str.includes('\n')) {
    const lines = str.split('\n');
    const parts = [];
    const limit = str.endsWith('\n') ? lines.length - 1 : lines.length;
    for (let i = 0; i < limit; i++) {
      const chunk = i === limit - 1 && !str.endsWith('\n') ? lines[i] : `${lines[i]}\n`;
      const suffix = i === limit - 1 ? '' : ' +';
      parts.push(`${i === 0 ? '' : '  '}${quoteString(chunk)}${suffix}`);
    }
    return parts.join('\n');
  }
  return quoteString(str);
}

function formatValue(value, options, depth, seen) {
  if (value === null) return 'null';
  const t = typeof value;
  if (t === 'string') return formatString(value, options);
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

  if (depth > options.depth && options.depth !== null) {
    // A custom formatter (e.g. Event) may still render at over-limit depth
    // (Node consults `inspect.custom` before applying the depth cutoff).
    const customAtLimit = value[inspect.custom];
    if (typeof customAtLimit === 'function') {
      const r = customAtLimit.call(value, options.depth, options);
      if (typeof r === 'string') return r;
    }
    if (Array.isArray(value)) return '[Array]';
    // A null-prototype object keeps its distinguishing tag even past the
    // depth limit, matching Node's `[Object: null prototype]` rendering.
    if (Object.getPrototypeOf(value) === null) return '[Object: null prototype]';
    return '[Object]';
  }

  // inspect.custom hook. Expose a recurse helper on the ctx so custom
  // inspectors (e.g. Buffer) can format nested values with these options.
  const custom = value[inspect.custom];
  if (typeof custom === 'function') {
    if (typeof options.inspect !== 'function') {
      options.inspect = (v) => formatValue(v, options, depth + 1, seen);
    }
    const r = custom.call(value, options.depth, options);
    if (typeof r === 'string') return r;
    if (r !== value) return formatValue(r, options, depth, seen);
  }

  seen.add(value);
  let out;
  try {
    if (types.isRegExp(value)) out = formatWrappedPrimitive(value, value.toString(), options, depth, seen);
    else if (types.isDate(value)) out = formatWrappedPrimitive(value, Number.isNaN(value.getTime()) ? 'Invalid Date' : value.toISOString(), options, depth, seen);
    else if (value instanceof Error) out = formatError(value, options, depth, seen);
    else if (Array.isArray(value)) out = formatArray(value, options, depth, seen);
    else if (types.isMap(value)) out = formatMap(value, options, depth, seen);
    else if (types.isSet(value)) out = formatSet(value, options, depth, seen);
    else if (types.isTypedArray(value)) out = formatTypedArray(value, options, depth, seen);
    else out = formatObject(value, options, depth, seen);
  } finally {
    seen.delete(value);
  }
  if (out.includes('[Circular *1]') && !out.startsWith('<ref *1> ')) {
    out = `<ref *1> ${out}`;
  }
  return out;
}

// §Date / RegExp — the primitive rendering (ISO string / `/re/flags`), prefixed
// with the constructor name when it is a subclass, plus any own enumerable
// expando properties as a trailing block (e.g. `MyDate 2016-...Z { '0': '1' }`).
function formatWrappedPrimitive(value, base, options, depth, seen) {
  const ctorName = value.constructor && value.constructor.name ? value.constructor.name : '';
  const tag = Object.prototype.toString.call(value).slice(8, -1);
  const prefixed = ctorName && ctorName !== tag ? `${ctorName} ${base}` : base;
  const parts = [];
  const keys = Object.keys(value);
  if (options.sorted) keys.sort();
  for (const k of keys) {
    parts.push(`${keyToString(k)}: ${formatValue(value[k], options, depth + 1, seen)}`);
  }
  for (const sym of ownEnumerableSymbols(value)) {
    parts.push(`${keyToString(sym)}: ${formatValue(value[sym], options, depth + 1, seen)}`);
  }
  if (parts.length === 0) return prefixed;
  return reduceToSingleString(parts, `${prefixed} `, ['{', '}'], options, depth);
}

// §Errors — `[Name: message]`, with non-enumerable `cause` and any extra own
// enumerable properties (but not the stack/message) shown as a trailing block.
// This is the structural form Node's assert diff and nested-value inspection
// use (the bare stack string is a separate top-level console concern).
function formatError(err, options, depth, seen) {
  const name = err.name || 'Error';
  const message = typeof err.message === 'string' ? err.message : '';
  const base = message ? `[${name}: ${message}]` : `[${name}]`;
  const parts = [];
  if ('cause' in err) {
    parts.push(`[cause]: ${formatValue(err.cause, options, depth + 1, seen)}`);
  }
  for (const key of Object.keys(err)) {
    if (key === 'stack' || key === 'message') continue;
    parts.push(`${keyToString(key)}: ${formatErrorProperty(err, key, options, depth, seen)}`);
  }
  if (parts.length === 0) return base;
  return reduceToSingleString(parts, `${base} `, ['{', '}'], options, depth);
}

function formatErrorProperty(err, key, options, depth, seen) {
  const value = err[key];
  if (err.name === 'AssertionError' && typeof value === 'string' &&
      (key === 'actual' || key === 'expected')) {
    return formatAssertionStringProperty(value);
  }
  return formatValue(value, options, depth + 1, seen);
}

function formatAssertionStringProperty(value) {
  if (value.includes('\n')) {
    const lines = value.split('\n');
    const limit = Math.min(10, value.endsWith('\n') ? lines.length - 1 : lines.length);
    const parts = [];
    for (let i = 0; i < limit; i++) {
      const chunk = `${lines[i]}\n`;
      parts.push(`${i === 0 ? '' : '    '}${quoteString(chunk)} +`);
    }
    if (lines.length - (value.endsWith('\n') ? 1 : 0) > limit) {
      parts.push(`    ${quoteString('...')}`);
    } else if (parts.length > 0) {
      parts[parts.length - 1] = parts[parts.length - 1].slice(0, -2);
    }
    return parts.join('\n');
  }
  if (value.length > 488) return quoteString(`${value.slice(0, 488)}...`);
  return quoteString(value);
}

function keyToString(key) {
  if (typeof key === 'symbol') return `[${key.toString()}]`;
  if (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(key)) return key;
  return quoteString(key);
}

function ownEnumerableSymbols(obj) {
  let symbols;
  try {
    symbols = Object.getOwnPropertySymbols(obj);
  } catch {
    return [];
  }
  const out = [];
  for (const sym of symbols) {
    const desc = Object.getOwnPropertyDescriptor(obj, sym);
    if (desc && desc.enumerable) out.push(sym);
  }
  return out;
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
  if (objToString(obj) === '[object Arguments]') {
    return '[Arguments] ';
  }
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

// §TypedArrays — `Ctor(len) [ ...elements ]`, with `[Tag]` when the
// @@toStringTag differs from the constructor name (e.g. Buffer →
// `Buffer(4) [Uint8Array]`), plus any extra non-index own properties.
function formatTypedArray(ta, options, depth, seen) {
  const ctorName = ta.constructor && ta.constructor.name ? ta.constructor.name : 'TypedArray';
  const tag = ta[Symbol.toStringTag];
  let prefix = `${ctorName}(${ta.length})`;
  if (typeof tag === 'string' && tag !== ctorName) prefix += ` [${tag}]`;
  prefix += ' ';
  const parts = [];
  const limit = Math.min(ta.length, options.maxArrayLength);
  for (let i = 0; i < limit; i++) {
    parts.push(formatValue(ta[i], options, depth + 1, seen));
  }
  if (ta.length > limit) {
    const extra = ta.length - limit;
    parts.push(`... ${extra} more item${extra > 1 ? 's' : ''}`);
  }
  for (const key of Object.keys(ta)) {
    if (/^(0|[1-9]\d*)$/.test(key)) continue;
    parts.push(`${keyToString(key)}: ${formatValue(ta[key], options, depth + 1, seen)}`);
  }
  for (const sym of ownEnumerableSymbols(ta)) {
    parts.push(`${keyToString(sym)}: ${formatValue(ta[sym], options, depth + 1, seen)}`);
  }
  return reduceToSingleString(parts, prefix, ['[', ']'], options, depth);
}

function formatObject(obj, options, depth, seen) {
  const parts = [];
  const keys = Object.keys(obj);
  if (options.sorted) keys.sort();
  for (const key of keys) {
    parts.push(`${keyToString(key)}: ${formatValue(obj[key], options, depth + 1, seen)}`);
  }
  for (const sym of ownEnumerableSymbols(obj)) {
    parts.push(`${keyToString(sym)}: ${formatValue(obj[sym], options, depth + 1, seen)}`);
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
// Circular-reference memo: a bidirectional (val1→position, val2→position)
// map with recursion-stack semantics (entries are deleted on exit). A pair
// (a, b) is a back-edge — and assumed equal — only when BOTH a and b were
// recorded at the SAME position on the current path, so a self-referential
// value can never vacuously match an unrelated one, and a repeated
// comparison of two distinct objects is re-evaluated rather than served a
// stale cached verdict.
function newMemo() {
  return { val1: new Map(), val2: new Map(), position: 0 };
}

function isDeepStrictEqual(a, b, skipPrototype) {
  return deepEqual(a, b, true, newMemo(), !!skipPrototype);
}
// Loose (`==`-based) structural equality — not a public Node API, exported for
// assert.deepEqual which shares util's comparison fidelity.
function isDeepEqual(a, b) {
  return deepEqual(a, b, false, newMemo(), false);
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
  if (isTypedArray(a)) {
    if (!isTypedArray(b)) return false;
    if (objToString(a) !== objToString(b)) return false;
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!Object.is(a[i], b[i])) return false;
    return compareKeys(a, b, strict, memo, skipProto, true);
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

  // §Circular references — a back-edge is a true cycle only when both a and
  // b were already recorded at the same position on the CURRENT recursion
  // path. Entries are popped on exit (below) so sibling comparisons of the
  // same object against a different partner are re-evaluated, never served a
  // stale verdict.
  const aPos = memo.val1.get(a);
  const bPos = memo.val2.get(b);
  if (aPos !== undefined || bPos !== undefined) {
    // A back-edge is a true cycle (equal) only when BOTH sides close
    // their cycle at the SAME position. If only one side is already on
    // the path, the actual graph loops where the expected graph still
    // descends (or vice-versa) — structurally different. `aPos === bPos`
    // captures all three: both-and-equal → true, both-and-different or
    // exactly-one-defined (number === undefined) → false.
    return aPos === bPos;
  }
  memo.position += 1;
  const position = memo.position;
  memo.val1.set(a, position);
  memo.val2.set(b, position);
  const result = deepEqualBody(a, b, strict, memo, skipProto);
  memo.val1.delete(a);
  memo.val2.delete(b);
  return result;
}

// The structural dispatch for two objects already pushed onto the
// circular-reference memo by `deepEqual`.
function deepEqualBody(a, b, strict, memo, skipProto) {
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
  for (const sym of ownEnumerableSymbols(obj)) keys.push(sym);
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
  reset: [0, 0], bold: [1, 22], dim: [2, 22], faint: [2, 22],
  italic: [3, 23], underline: [4, 24], inverse: [7, 27], hidden: [8, 28],
  strikethrough: [9, 29],
  black: [30, 39],
  red: [31, 39], green: [32, 39], yellow: [33, 39], blue: [34, 39],
  magenta: [35, 39], cyan: [36, 39], white: [37, 39], gray: [90, 39],
  grey: [90, 39], blackBright: [90, 39], redBright: [91, 39],
  greenBright: [92, 39], yellowBright: [93, 39], blueBright: [94, 39],
  magentaBright: [95, 39], cyanBright: [96, 39], whiteBright: [97, 39],
  bgBlack: [40, 49], bgRed: [41, 49], bgGreen: [42, 49], bgYellow: [43, 49],
  bgBlue: [44, 49], bgMagenta: [45, 49], bgCyan: [46, 49], bgWhite: [47, 49],
  bgGray: [100, 49], bgGrey: [100, 49],
};
function styleText(format, text, options = {}) {
  if (typeof text !== 'string') {
    const err = new TypeError('The "text" argument must be of type string.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  const formats = Array.isArray(format) ? format : [format];
  const resolved = [];
  for (const f of formats) {
    if (f === 'none') continue;
    if (typeof f === 'string' && f.charCodeAt(0) === 35) {
      const digits = f.slice(1);
      const validLength = digits.length === 3 || digits.length === 6;
      let valid = validLength;
      for (let i = 0; i < digits.length; i++) {
        const code = digits.charCodeAt(i);
        if (!((code >= 48 && code <= 57) || (code >= 65 && code <= 70) ||
              (code >= 97 && code <= 102))) valid = false;
      }
      if (!valid) {
        const err = new TypeError(`The value "${String(f)}" must be a valid hex color.`);
        err.code = 'ERR_INVALID_ARG_VALUE';
        throw err;
      }
      const full = digits.length === 3 ? digits.split('').map((c) => c + c).join('') : digits;
      resolved.push([`38;2;${parseInt(full.slice(0, 2), 16)};${parseInt(full.slice(2, 4), 16)};${parseInt(full.slice(4, 6), 16)}`, 39]);
    } else if (!Object.prototype.hasOwnProperty.call(styleCodes, f)) {
      const err = new TypeError(`The value "${String(f)}" is invalid for argument 'format'.`);
      err.code = 'ERR_INVALID_ARG_VALUE';
      throw err;
    } else {
      resolved.push(styleCodes[f]);
    }
  }
  if (options && options.validateStream !== false) {
    const stream = options.stream || (typeof process !== 'undefined' ? process.stdout : null);
    if (!stream || typeof stream !== 'object') {
      const err = new TypeError('The "stream" argument must be of type object.');
      err.code = 'ERR_INVALID_ARG_TYPE';
      throw err;
    }
    if (options.stream && typeof stream.hasColors !== 'function') {
      const err = new TypeError('The "stream" argument must be a TTY stream.');
      err.code = 'ERR_INVALID_ARG_TYPE';
      throw err;
    }
    const env = typeof process !== 'undefined' && process.env ? process.env : {};
    if (env.FORCE_COLOR === '0') return text;
    const forceColor = env.FORCE_COLOR && env.FORCE_COLOR !== '0';
    if (!forceColor && (env.NODE_DISABLE_COLORS || env.NO_COLOR)) return text;
    if (!stream.isTTY && !forceColor) {
      return text;
    }
  }
  let output = text;
  for (let i = resolved.length - 1; i >= 0; i--) {
    const [open, close] = resolved[i];
    const openCode = `\x1b[${open}m`;
    const closeCode = `\x1b[${close}m`;
    const trailingClose = output.endsWith(closeCode);
    const body = trailingClose ? output.slice(0, -closeCode.length) : output;
    const reopen = close === 22 ? closeCode + openCode : openCode;
    output = openCode + body.split(closeCode).join(reopen) +
      (trailingClose ? closeCode : '') + closeCode;
  }
  return output;
}

// ---------- misc ----------
function debuglog() {
  const fn = () => {};
  fn.enabled = false;
  return fn;
}

function utf8ToBytes(str) {
  const out = [];
  str = String(str);
  for (let i = 0; i < str.length; i++) {
    let code = str.charCodeAt(i);
    if (code >= 0xd800 && code <= 0xdbff && i + 1 < str.length) {
      const next = str.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        code = 0x10000 + ((code - 0xd800) << 10) + (next - 0xdc00);
        i++;
      } else {
        code = 0xfffd;
      }
    } else if (code >= 0xd800 && code <= 0xdfff) {
      code = 0xfffd;
    }
    if (code < 0x80) out.push(code);
    else if (code < 0x800) out.push(0xc0 | (code >> 6), 0x80 | (code & 0x3f));
    else if (code < 0x10000) out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f));
    else out.push(0xf0 | (code >> 18), 0x80 | ((code >> 12) & 0x3f), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f));
  }
  return out;
}

// Parse Node's dotenv grammar without consulting process.env. Quoted values
// may span lines, comments only start outside quotes, and only double-quoted
// values expand the conventional `\n` and `\r` escapes.
function parseEnv(content) {
  if (typeof content !== 'string') {
    throw argTypeError('content', 'argument must be of type string', content);
  }
  const result = {};
  let index = 0;
  while (index < content.length) {
    while (index < content.length && /[ \t\r\n]/.test(content[index])) index++;
    if (index >= content.length) break;
    if (content[index] === '#') {
      while (index < content.length && content[index] !== '\n') index++;
      continue;
    }
    const lineStart = index;
    if (content.slice(index, index + 6) === 'export' && /[ \t]/.test(content[index + 6] || '')) {
      index += 6;
      while (content[index] === ' ' || content[index] === '\t') index++;
    }
    const keyStart = index;
    while (index < content.length && content[index] !== '=' &&
           content[index] !== '\n' && content[index] !== '#') index++;
    if (content[index] !== '=') {
      index = lineStart;
      while (index < content.length && content[index] !== '\n') index++;
      continue;
    }
    const key = content.slice(keyStart, index).trim();
    index++;
    while (content[index] === ' ' || content[index] === '\t') index++;
    let value = '';
    const quote = content[index];
    if (quote === '"' || quote === "'" || quote === '`') {
      const valueStart = ++index;
      const closing = content.indexOf(quote, valueStart);
      if (closing === -1) {
        let lineEnd = content.indexOf('\n', valueStart);
        if (lineEnd === -1) lineEnd = content.length;
        value = quote + content.slice(valueStart, lineEnd).trimEnd();
        index = lineEnd;
      } else {
        value = content.slice(valueStart, closing);
        index = closing + 1;
        if (quote === '"') value = value.split('\\n').join('\n').split('\\r').join('\r');
        while (index < content.length && content[index] !== '\n') index++;
      }
    } else {
      const valueStart = index;
      while (index < content.length && content[index] !== '\n' && content[index] !== '#') index++;
      value = content.slice(valueStart, index).trim();
      while (index < content.length && content[index] !== '\n') index++;
    }
    if (key) result[key] = value;
  }
  return result;
}

function toUSVString(value) {
  const input = String(value);
  let output = '';
  for (let index = 0; index < input.length; index++) {
    const first = input.charCodeAt(index);
    if (first < 0xd800 || first > 0xdfff) {
      output += input[index];
    } else if (first <= 0xdbff && index + 1 < input.length) {
      const second = input.charCodeAt(index + 1);
      if (second >= 0xdc00 && second <= 0xdfff) output += input[index] + input[++index];
      else output += '\ufffd';
    } else {
      output += '\ufffd';
    }
  }
  return output;
}

class TextEncoder {
  get encoding() { return 'utf-8'; }
  encode(input = '') { return new Uint8Array(utf8ToBytes(input)); }
}

const exportsObj = {
  inspect,
  format,
  formatWithOptions,
  types,
  isDeepStrictEqual,
  isDeepEqual,
  promisify,
  callbackify,
  inherits,
  deprecate,
  debuglog,
  debug: debuglog,
  stripVTControlCharacters,
  styleText,
  TextEncoder,
  parseEnv,
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
  toUSVString,
  getSystemErrorName(err) { return `Unknown system error ${err}`; },
  getCallSites(frameCountOrOptions, options) {
    // Node signature: getCallSites([frameCount][, options]). We support
    // the numeric frame count; default 10. `sourceMap` option is not yet
    // honoured. Skip 1 frame so the first call site is the caller, not
    // this wrapper.
    let frameCount = 10;
    if (typeof frameCountOrOptions === 'number') {
      frameCount = frameCountOrOptions;
    } else if (frameCountOrOptions && typeof frameCountOrOptions === 'object') {
      options = frameCountOrOptions;
    }
    return JSON.parse(__captureCallSites(1, frameCount));
  },
  _extend(target, source) {
    if (source === null || typeof source !== 'object') return target;
    for (const k of Object.keys(source)) target[k] = source[k];
    return target;
  },
};

module.exports = exportsObj;
