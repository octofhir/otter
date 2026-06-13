'use strict';
// `node:assert` — JS implementation. The deep-equality and value rendering come
// from `util` (injected as a dependency); the rest is the assertion surface the
// test suite relies on, including a real `AssertionError` class (correct
// `name`/`code`/`actual`/`expected`/`operator`) so matcher checks observe it.

const util = require('util');
const { isDeepStrictEqual, inspect } = util;

function inspectValue(v) {
  return inspect(v, { depth: null, breakLength: Infinity, compact: 3 });
}

class AssertionError extends Error {
  constructor(options = {}) {
    const { message, actual, expected, operator, stackStartFn } = options;
    let msg = message;
    let generatedMessage = false;
    if (msg === undefined) {
      generatedMessage = true;
      if (operator === 'fail') {
        msg = 'Failed';
      } else {
        const op = operator || 'deepStrictEqual';
        msg = `${inspectValue(actual)} ${op} ${inspectValue(expected)}`;
      }
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

function looseDeepEqual(a, b, seen) {
  // eslint-disable-next-line eqeqeq
  if (a == b) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) {
    // eslint-disable-next-line eqeqeq
    return a == b;
  }
  if (seen.has(a)) return true;
  seen.add(a);
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
  if (!looseDeepEqual(actual, expected, new Set())) {
    innerFail({ actual, expected, message, operator: 'deepEqual', stackStartFn: deepEqual });
  }
}
function notDeepEqual(actual, expected, message) {
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

function fail(message) {
  if (message instanceof Error) throw message;
  innerFail({
    actual: undefined, expected: undefined,
    message: message === undefined ? undefined : message,
    operator: 'fail', stackStartFn: fail,
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
    // string is a message, not a matcher
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
    // validator function
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
      // also compare name/message
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
    // If a filter is provided and does not match, rethrow.
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
    throw new TypeError('The "regexp" argument must be an instance of RegExp.');
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
    throw new TypeError('The "regexp" argument must be an instance of RegExp.');
  }
  if (typeof string === 'string' && regexp.test(string)) {
    innerFail({ actual: string, expected: regexp, message, operator: 'doesNotMatch', stackStartFn: doesNotMatch });
  }
}

// ---- CallTracker (deprecated, minimal) ----
class CallTracker {
  constructor() { this._calls = []; }
  calls(fn, exact) {
    if (typeof fn === 'number') { exact = fn; fn = () => {}; }
    if (fn === undefined) fn = () => {};
    if (exact === undefined) exact = 1;
    const record = { name: fn.name || '<anonymous>', actual: 0, expected: exact };
    this._calls.push(record);
    const tracked = (...args) => { record.actual++; return fn.apply(this, args); };
    return tracked;
  }
  getCalls() { return []; }
  report() {
    return this._calls
      .filter((c) => c.actual !== c.expected)
      .map((c) => ({ message: `Expected the ${c.name} function to be called ${c.expected} times but was called ${c.actual} times.`, actual: c.actual, expected: c.expected, operator: c.name }));
  }
  verify() {
    const failed = this._calls.filter((c) => c.actual !== c.expected);
    if (failed.length > 0) {
      const err = new AssertionError({
        message: `Functions were not called the expected number of times`,
        operator: 'CallTracker',
      });
      throw err;
    }
  }
  reset() { this._calls = []; }
}

// ---- assemble ----
assert.ok = ok;
assert.assert = assert;
assert.equal = equal;
assert.notEqual = notEqual;
assert.strictEqual = strictEqual;
assert.notStrictEqual = notStrictEqual;
assert.deepEqual = deepEqual;
assert.notDeepEqual = notDeepEqual;
assert.deepStrictEqual = deepStrictEqual;
assert.notDeepStrictEqual = notDeepStrictEqual;
assert.partialDeepStrictEqual = partialDeepStrictEqual;
assert.throws = throws;
assert.doesNotThrow = doesNotThrow;
assert.rejects = rejects;
assert.doesNotReject = doesNotReject;
assert.ifError = ifError;
assert.fail = fail;
assert.match = match;
assert.doesNotMatch = doesNotMatch;
assert.AssertionError = AssertionError;
assert.CallTracker = CallTracker;
// The default comparison helpers are already strict, so the `strict` surface
// mirrors the namespace itself (and is callable, like `assert`).
assert.strict = assert;
assert.strict.strict = assert;

module.exports = assert;
