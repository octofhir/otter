'use strict';
// `node:assert` — JS surface. Deep-equality and value rendering come from `util`
// (injected); CallTracker and the Myers diff live in injected internal modules
// (internal/assert/calltracker, internal/assert/myers_diff). A real
// `AssertionError` class carries the correct name/code/actual/expected/operator
// so matcher checks observe it; the `Assert` class is the constructible form.

const util = require('util');
const { isDeepStrictEqual, inspect } = util;
const makeCallTracker = require('internal/assert/calltracker');

function inspectValue(v) {
  return inspect(v, { depth: null, breakLength: Infinity, compact: 3 });
}

// Strict operators render a line-by-line +/- diff; loose operators render the
// two values around a "should (not) loosely (deep-)equal" separator.
const kDiffOperators = new Set([
  'deepStrictEqual', 'notDeepStrictEqual', 'partialDeepStrictEqual',
  'strictEqual', 'notStrictEqual',
]);
const kLooseOperators = {
  deepEqual: 'should loosely deep-equal',
  notDeepEqual: 'should not loosely deep-equal',
  equal: 'should loosely equal',
  notEqual: 'should not loosely equal',
};
const kLooseHeaders = {
  deepEqual: 'Expected values to be loosely deep-equal:',
  notDeepEqual: 'Expected "actual" not to be loosely deep-equal to:',
  equal: 'Expected values to be loosely equal:',
  notEqual: 'Expected "actual" not to be loosely equal to:',
};
function looseDiffMessage(actual, expected, operator) {
  const opts = {
    compact: false, depth: 1000, customInspect: false,
    maxArrayLength: Infinity, breakLength: Infinity, sorted: true, getters: true,
  };
  return `${kLooseHeaders[operator]}\n\n${inspect(actual, opts)}\n\n` +
    `${kLooseOperators[operator]}\n\n${inspect(expected, opts)}`;
}

// Per-operator header for a generated diff message (replaced by a custom
// message when one is supplied).
const kDiffHeaders = {
  strictEqual: 'Expected values to be strictly equal:',
  notStrictEqual: 'Expected "actual" to be strictly unequal to:',
  deepStrictEqual: 'Expected values to be strictly deep-equal:',
  notDeepStrictEqual: 'Expected "actual" not to be strictly deep-equal to:',
  deepEqual: 'Expected values to be loosely deep-equal:',
  notDeepEqual: 'Expected "actual" not to be loosely deep-equal to:',
  equal: 'Expected values to be loosely equal:',
  partialDeepStrictEqual: 'Expected values to be partially and strictly deep-equal:',
};

// Two inspected lines are "the same" for diff purposes if they are equal or
// differ only by a trailing comma (the last element of a block loses its comma
// when it has no following sibling). Node keeps the comma'd form so an added
// sibling shows as a single inserted line, not a remove+add of the element.
function lineEq(a, b) {
  return a === b ||
    (a.endsWith(',') && a.slice(0, -1) === b) ||
    (b.endsWith(',') && b.slice(0, -1) === a);
}
function commonLine(a, b) {
  return a.endsWith(',') ? a : b;
}

// §createErrDiff — line-by-line LCS of inspect(actual)/inspect(expected) (both
// compact:false so each entry is its own line), prefixed `  ` (common) / `+ `
// (actual) / `- ` (expected) under a "+ actual - expected" legend. Comma-aware
// (see lineEq), mirroring Node's lib/internal/assert/assertion_error.js.
function diffLines(a, e) {
  const n = a.length;
  const m = e.length;
  const dp = [];
  for (let i = 0; i <= n; i++) dp.push(new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = lineEq(a[i], e[j])
        ? dp[i + 1][j + 1] + 1
        : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const out = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (lineEq(a[i], e[j])) { out.push(`  ${commonLine(a[i], e[j])}`); i++; j++; } else if (dp[i + 1][j] >= dp[i][j + 1]) { out.push(`+ ${a[i]}`); i++; } else { out.push(`- ${e[j]}`); j++; }
  }
  while (i < n) out.push(`+ ${a[i++]}`);
  while (j < m) out.push(`- ${e[j++]}`);
  return out;
}

function createErrDiff(actual, expected, prefix) {
  const opts = {
    compact: false, breakLength: Infinity, depth: 1000,
    customInspect: false, sorted: true,
  };
  const actualLines = inspect(actual, opts).split('\n');
  const expectedLines = inspect(expected, opts).split('\n');
  const lines = diffLines(actualLines, expectedLines);
  return `${prefix}\n+ actual - expected\n\n${lines.join('\n')}\n`;
}

class AssertionError extends Error {
  constructor(options = {}) {
    const { message, actual, expected, operator, stackStartFn } = options;
    let msg = message;
    let generatedMessage = false;
    const wantsDiff = kDiffOperators.has(operator) &&
      !(typeof operator === 'string' && operator.startsWith('not'));
    const wantsLoose = Object.prototype.hasOwnProperty.call(kLooseOperators, operator);
    if (msg === undefined) {
      generatedMessage = true;
      if (operator === 'fail') {
        msg = 'Failed';
      } else if (wantsDiff) {
        msg = createErrDiff(actual, expected, kDiffHeaders[operator] || '');
      } else if (wantsLoose) {
        msg = looseDiffMessage(actual, expected, operator);
      } else {
        const op = operator || 'deepStrictEqual';
        msg = `${inspectValue(actual)} ${op} ${inspectValue(expected)}`;
      }
    } else if (wantsDiff) {
      // An explicit message replaces the header but keeps the diff.
      msg = createErrDiff(actual, expected, message);
    }
    super(msg);
    this.name = 'AssertionError';
    this.code = 'ERR_ASSERTION';
    this.actual = actual;
    this.expected = expected;
    this.operator = operator;
    this.generatedMessage = generatedMessage;
    if (Error.captureStackTrace && stackStartFn) {
      Error.captureStackTrace(this, stackStartFn);
    }
  }

  get [Symbol.toStringTag]() { return 'Error'; }
}

function innerFail(obj) {
  throw new AssertionError(obj);
}

// §validateArgumentCount — the equality helpers require both `actual` and
// `expected`; a single argument is ERR_MISSING_ARGS, mirroring Node.
function requireTwoArgs(len) {
  if (len < 2) {
    const e = new TypeError(
      'The "actual" and "expected" arguments must be specified'
    );
    e.code = 'ERR_MISSING_ARGS';
    throw e;
  }
}

function regexpArgError(value) {
  const t = typeof value;
  const received =
    value === null ? 'null'
      : value === undefined ? 'undefined'
        : t === 'string' ? `type string ('${value}')`
          : t === 'object' ? `an instance of ${value.constructor ? value.constructor.name : 'Object'}`
            : `type ${t} (${String(value)})`;
  const e = new TypeError(
    `The "regexp" argument must be an instance of RegExp. Received ${received}`
  );
  e.code = 'ERR_INVALID_ARG_TYPE';
  return e;
}

function ok(...args) {
  const value = args[0];
  if (!value) {
    innerFail({
      actual: value,
      expected: true,
      message: args.length > 1 ? args[1] : undefined,
      operator: '==',
      stackStartFn: ok,
    });
  }
}

function assert(...args) {
  ok(...args);
}

function strictEqual(actual, expected, message) {
  if (!Object.is(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'strictEqual', stackStartFn: strictEqual });
  }
}
function notStrictEqual(actual, expected, message) {
  if (Object.is(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'notStrictEqual', stackStartFn: notStrictEqual });
  }
}
function equal(actual, expected, message) {
  // eslint-disable-next-line eqeqeq
  if (actual != expected) {
    innerFail({ actual, expected, message, operator: '==', stackStartFn: equal });
  }
}
function notEqual(actual, expected, message) {
  // eslint-disable-next-line eqeqeq
  if (actual == expected) {
    innerFail({ actual, expected, message, operator: '!=', stackStartFn: notEqual });
  }
}

function deepStrictEqual(actual, expected, message) {
  if (!isDeepStrictEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'deepStrictEqual', stackStartFn: deepStrictEqual });
  }
}
function notDeepStrictEqual(actual, expected, message) {
  if (isDeepStrictEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'notDeepStrictEqual', stackStartFn: notDeepStrictEqual });
  }
}

// §isDeepEqual (loose) — like isDeepStrictEqual but primitives compare with ==
// and only enumerable own keys are considered. Handles Map/Set/Date/RegExp and
// boxed primitives the way the suite expects.
function tag(v) { return Object.prototype.toString.call(v); }
function looseDeepEqual(a, b, seen) {
  // eslint-disable-next-line eqeqeq
  if (a == b) return true;
  if (typeof a === 'number' && typeof b === 'number') {
    return Number.isNaN(a) && Number.isNaN(b);
  }
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) {
    // eslint-disable-next-line eqeqeq
    return a == b;
  }
  const ta = tag(a);
  if (ta !== tag(b)) return false;
  if (ta === '[object Date]') return a.getTime() === b.getTime();
  if (ta === '[object RegExp]') return a.source === b.source && a.flags === b.flags;
  if (a instanceof Error && b instanceof Error) {
    if (a.name !== b.name || a.message !== b.message) return false;
  }
  if (seen.has(a)) return true;
  seen.add(a);
  if (ta === '[object Map]') {
    if (a.size !== b.size) return false;
    for (const [k, v] of a) {
      if (!b.has(k) || !looseDeepEqual(v, b.get(k), seen)) return false;
    }
    return true;
  }
  if (ta === '[object Set]') {
    if (a.size !== b.size) return false;
    for (const v of a) if (!b.has(v)) return false;
    return true;
  }
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  for (const k of ka) {
    if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
    if (!looseDeepEqual(a[k], b[k], seen)) return false;
  }
  return true;
}
function deepEqual(actual, expected, message) {
  requireTwoArgs(arguments.length);
  if (!looseDeepEqual(actual, expected, new Set())) {
    innerFail({ actual, expected, message, operator: 'deepEqual', stackStartFn: deepEqual });
  }
}
function notDeepEqual(actual, expected, message) {
  requireTwoArgs(arguments.length);
  if (looseDeepEqual(actual, expected, new Set())) {
    innerFail({ actual, expected, message, operator: 'notDeepEqual', stackStartFn: notDeepEqual });
  }
}

function partialDeepStrictEqual(actual, expected, message) {
  if (!partialMatch(actual, expected, new Set())) {
    innerFail({ actual, expected, message, operator: 'partialDeepStrictEqual', stackStartFn: partialDeepStrictEqual });
  }
}
function partialMatch(actual, expected, seen) {
  if (Object.is(actual, expected)) return true;
  if (typeof expected !== 'object' || expected === null) return Object.is(actual, expected);
  if (typeof actual !== 'object' || actual === null) return false;
  if (seen.has(expected)) return true;
  seen.add(expected);
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual)) return false;
    return expected.every((e) => actual.some((a) => partialMatch(a, e, seen)));
  }
  for (const k of Reflect.ownKeys(expected)) {
    if (!Reflect.has(actual, k)) return false;
    if (!partialMatch(actual[k], expected[k], seen)) return false;
  }
  return true;
}

function ifError(value) {
  if (value !== null && value !== undefined) {
    let message = 'ifError got unwanted exception: ';
    if (typeof value === 'object' && typeof value.message === 'string') {
      message += value.message.length === 0 && value.constructor ? value.constructor.name : value.message;
    } else {
      message += inspectValue(value);
    }
    const err = new AssertionError({
      actual: value, expected: null, operator: 'ifError', message, stackStartFn: ifError,
    });
    err.generatedMessage = false;
    throw err;
  }
}

// §assert.fail — the multi-argument form is deprecated; with exactly two
// arguments the operator defaults to "!=" and the message is generated.
function fail(actual, expected, message, operator, stackStartFn) {
  const argsLen = arguments.length;
  if (argsLen === 1) {
    // fail(message) — the lone argument is the message.
    message = actual;
    actual = undefined;
  } else if (argsLen === 2) {
    // Deprecated two-argument form: operator defaults to "!=" and the
    // message is generated by AssertionError (generatedMessage stays true).
    operator = '!=';
  }
  if (message instanceof Error) throw message;
  innerFail({
    actual,
    expected,
    message,
    operator: operator || 'fail',
    stackStartFn: stackStartFn || fail,
  });
}

// ---- throws / rejects ----
function isErrorConstructor(fn) {
  if (typeof fn !== 'function') return false;
  let proto = fn.prototype;
  while (proto) {
    if (proto === Error.prototype) return true;
    proto = Object.getPrototypeOf(proto);
  }
  return /^[A-Z]/.test(fn.name) && fn.name.endsWith('Error');
}

function expectedException(actual, expected, message, fn) {
  if (typeof expected === 'string') {
    throw new AssertionError({
      actual, expected: undefined, operator: fn.name,
      message: `Got unwanted exception${expected ? `: ${expected}` : ''}`,
      stackStartFn: fn,
    });
  }
  if (expected === undefined) return true;
  if (typeof expected === 'function' && isErrorConstructor(expected)) {
    if (actual instanceof expected) return true;
    throw new AssertionError({
      actual, expected, operator: fn.name,
      message: message || `The error is expected to be an instance of "${expected.name}". Received "${actual && actual.constructor ? actual.constructor.name : typeof actual}"`,
      stackStartFn: fn,
    });
  }
  if (expected instanceof RegExp) {
    const str = actual && typeof actual.message === 'string' ? actual.message : String(actual);
    if (expected.test(str)) return true;
    throw new AssertionError({
      actual, expected, operator: fn.name,
      message: message || `The input did not match the regular expression ${expected}. Input: '${str}'`,
      stackStartFn: fn,
    });
  }
  if (typeof expected === 'function') {
    if (expected.call({}, actual) === true) return true;
    throw new AssertionError({
      actual, expected, operator: fn.name,
      message: message || 'The validation function is expected to return "true".',
      stackStartFn: fn,
    });
  }
  if (typeof expected === 'object' && expected !== null) {
    for (const key of Object.keys(expected)) {
      const want = expected[key];
      const got = actual ? actual[key] : undefined;
      if (want instanceof RegExp) {
        if (!want.test(String(got))) {
          throw new AssertionError({ actual: got, expected: want, operator: fn.name, message, stackStartFn: fn });
        }
      } else if (!isDeepStrictEqual(got, want)) {
        throw new AssertionError({
          actual, expected, operator: fn.name,
          message: message || `Expected the "${key}" property to match.`,
          stackStartFn: fn,
        });
      }
    }
    if (expected instanceof Error) {
      if (actual && actual.name === expected.name && actual.message === expected.message) return true;
    }
    return true;
  }
  return true;
}

function throws(fn, ...rest) {
  if (typeof fn !== 'function') {
    throw new TypeError('The "fn" argument must be of type function.');
  }
  let expected; let message;
  if (rest.length >= 1) {
    if (typeof rest[0] === 'string') message = rest[0];
    else { expected = rest[0]; message = rest[1]; }
  }
  let thrown;
  let didThrow = false;
  try { fn(); } catch (e) { didThrow = true; thrown = e; }
  if (!didThrow) {
    throw new AssertionError({
      actual: undefined, expected, operator: 'throws',
      message: message ? `Missing expected exception: ${message}` : 'Missing expected exception.',
      stackStartFn: throws,
    });
  }
  expectedException(thrown, expected, message, throws);
}

function doesNotThrow(fn, ...rest) {
  let expected; let message;
  if (typeof rest[0] === 'string') message = rest[0];
  else { expected = rest[0]; message = rest[1]; }
  let thrown; let didThrow = false;
  try { fn(); } catch (e) { didThrow = true; thrown = e; }
  if (didThrow) {
    if (expected && !matchesFilter(thrown, expected)) throw thrown;
    throw new AssertionError({
      actual: thrown, expected, operator: 'doesNotThrow',
      message: `Got unwanted exception${message ? `: ${message}` : '.'}\n${thrown && thrown.message ? thrown.message : ''}`,
      stackStartFn: doesNotThrow,
    });
  }
}

function matchesFilter(actual, expected) {
  if (typeof expected === 'function' && isErrorConstructor(expected)) return actual instanceof expected;
  if (expected instanceof RegExp) return expected.test(actual && actual.message ? actual.message : String(actual));
  return true;
}

async function rejects(promiseFn, ...rest) {
  let expected; let message;
  if (typeof rest[0] === 'string') message = rest[0];
  else { expected = rest[0]; message = rest[1]; }
  let thrown; let didReject = false;
  try {
    const p = typeof promiseFn === 'function' ? promiseFn() : promiseFn;
    await p;
  } catch (e) { didReject = true; thrown = e; }
  if (!didReject) {
    throw new AssertionError({
      actual: undefined, expected, operator: 'rejects',
      message: message ? `Missing expected rejection: ${message}` : 'Missing expected rejection.',
      stackStartFn: rejects,
    });
  }
  expectedException(thrown, expected, message, rejects);
}

async function doesNotReject(promiseFn, ...rest) {
  let expected; let message;
  if (typeof rest[0] === 'string') message = rest[0];
  else { expected = rest[0]; message = rest[1]; }
  let thrown; let didReject = false;
  try {
    const p = typeof promiseFn === 'function' ? promiseFn() : promiseFn;
    await p;
  } catch (e) { didReject = true; thrown = e; }
  if (didReject) {
    if (expected && !matchesFilter(thrown, expected)) throw thrown;
    throw new AssertionError({
      actual: thrown, expected, operator: 'doesNotReject',
      message: `Got unwanted rejection.\n${thrown && thrown.message ? thrown.message : ''}`,
      stackStartFn: doesNotReject,
    });
  }
}

function match(string, regexp, message) {
  if (!(regexp instanceof RegExp)) {
    throw regexpArgError(regexp);
  }
  if (typeof string !== 'string') {
    throw new AssertionError({ actual: string, expected: regexp, operator: 'match', message, stackStartFn: match });
  }
  if (!regexp.test(string)) {
    innerFail({ actual: string, expected: regexp, message, operator: 'match', stackStartFn: match });
  }
}
function doesNotMatch(string, regexp, message) {
  if (!(regexp instanceof RegExp)) {
    throw regexpArgError(regexp);
  }
  if (typeof string === 'string' && regexp.test(string)) {
    innerFail({ actual: string, expected: regexp, message, operator: 'doesNotMatch', stackStartFn: doesNotMatch });
  }
}

const CallTracker = makeCallTracker(AssertionError);

// ---- assemble the standalone surface ----
function assignMethods(target) {
  target.ok = ok;
  target.equal = equal;
  target.notEqual = notEqual;
  target.strictEqual = strictEqual;
  target.notStrictEqual = notStrictEqual;
  target.deepEqual = deepEqual;
  target.notDeepEqual = notDeepEqual;
  target.deepStrictEqual = deepStrictEqual;
  target.notDeepStrictEqual = notDeepStrictEqual;
  target.partialDeepStrictEqual = partialDeepStrictEqual;
  target.throws = throws;
  target.doesNotThrow = doesNotThrow;
  target.rejects = rejects;
  target.doesNotReject = doesNotReject;
  target.ifError = ifError;
  target.fail = fail;
  target.match = match;
  target.doesNotMatch = doesNotMatch;
  return target;
}

// §Assert — the constructible form. `new Assert({ strict, diff })` yields an
// object carrying the assertion methods; calling without `new` throws
// ERR_CONSTRUCT_CALL_REQUIRED (a plain function, not an ES class, so the code
// is ours to set rather than the engine's bare "requires new").
const kValidDiff = new Set([undefined, 'simple', 'full']);

function Assert(options) {
  if (!(this instanceof Assert)) {
    const e = new TypeError("Class constructor Assert cannot be invoked without 'new'");
    e.code = 'ERR_CONSTRUCT_CALL_REQUIRED';
    throw e;
  }
  options = options || {};
  const diffMode = options.diff;
  if (!kValidDiff.has(diffMode)) {
    const e = new TypeError(
      `The property 'options.diff' must be one of: 'simple', 'full'. ` +
        `Received ${inspectValue(diffMode)}`
    );
    e.code = 'ERR_INVALID_ARG_VALUE';
    throw e;
  }
  const strict = options.strict !== false;
  this.strict = strict;

  // Tag any AssertionError thrown through this instance with its diff mode.
  const wrap = diffMode === undefined ? (fn) => fn : (fn) => function wrapped(...args) {
    try { return fn.apply(this, args); } catch (e) {
      if (e instanceof AssertionError && e.diff === undefined) e.diff = diffMode;
      throw e;
    }
  };

  // Strict instances alias the loose comparators to the strict ones (so
  // `instance.equal === instance.strictEqual`).
  const strictEqualM = wrap(strictEqual);
  const notStrictEqualM = wrap(notStrictEqual);
  const deepStrictEqualM = wrap(deepStrictEqual);
  const notDeepStrictEqualM = wrap(notDeepStrictEqual);

  this.ok = wrap(ok);
  this.strictEqual = strictEqualM;
  this.notStrictEqual = notStrictEqualM;
  this.deepStrictEqual = deepStrictEqualM;
  this.notDeepStrictEqual = notDeepStrictEqualM;
  this.equal = strict ? strictEqualM : wrap(equal);
  this.notEqual = strict ? notStrictEqualM : wrap(notEqual);
  this.deepEqual = strict ? deepStrictEqualM : wrap(deepEqual);
  this.notDeepEqual = strict ? notDeepStrictEqualM : wrap(notDeepEqual);
  this.partialDeepStrictEqual = wrap(partialDeepStrictEqual);
  this.throws = throws;
  this.doesNotThrow = doesNotThrow;
  this.rejects = rejects;
  this.doesNotReject = doesNotReject;
  this.ifError = ifError;
  this.fail = wrap(fail);
  this.match = match;
  this.doesNotMatch = doesNotMatch;
  this.AssertionError = AssertionError;
  this.CallTracker = CallTracker;
}
Assert.prototype.constructor = Assert;

assignMethods(assert);
assert.assert = assert;
assert.AssertionError = AssertionError;
assert.CallTracker = CallTracker;
assert.Assert = Assert;
// The default comparison helpers are already strict, so the `strict` surface
// mirrors the namespace itself (and is callable, like `assert`).
assert.strict = assert;
assert.strict.strict = assert;

module.exports = assert;
