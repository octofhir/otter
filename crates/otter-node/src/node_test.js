'use strict';
// Minimal `node:test` runner shim — enough to execute Node's own
// `test/parallel` files that drive their assertions through `node:test`.
//
// Semantics that matter for conformance:
// - `test(name, fn)` runs `fn` immediately (sync) or attaches handlers (async).
// - A thrown error or a rejected promise marks the run failed and sets
//   `process.exitCode = 1`, which the conformance harness reads as failure.
// - All-pass leaves the exit code untouched (0).
//
// The test context `t` exposes the `assert` surface plus sub-`test`, so files
// written as `test('x', (t) => { t.assert.ok(...) })` and the older
// `test('x', () => { assert.ok(...) })` both work.

const assert = require('assert');

let failures = 0;

function fail(name, err) {
  failures += 1;
  if (typeof process !== 'undefined') process.exitCode = 1;
  const label = name ? `not ok - ${name}` : 'not ok';
  const detail = err && err.stack ? err.stack : String(err);
  try {
    console.error(`${label}\n${detail}`);
  } catch {
    // console may be unavailable; the exit code still signals failure.
  }
}

function isThenable(v) {
  return v != null && typeof v.then === 'function';
}


// ---- mocking (node:test MockTracker) ----

// A mock replaces a function or a property and records what was called with
// what, so a test can both stub behaviour and assert on it. Every mock a
// tracker hands out is remembered, which is what lets `restoreAll` put the
// originals back.

class MockFunctionContext {
  #calls = [];
  #implementation;
  #onceImplementations = new Map();
  #restore;
  #times;

  constructor(implementation, restore, times) {
    this.#implementation = implementation;
    this.#restore = restore;
    this.#times = times;
  }

  get calls() { return this.#calls.slice(); }

  callCount() { return this.#calls.length; }

  mockImplementation(implementation) {
    this.#implementation = implementation;
  }

  mockImplementationOnce(implementation, onCall) {
    const at = onCall ?? this.#calls.length;
    this.#onceImplementations.set(at, implementation);
  }

  resetCalls() { this.#calls = []; }

  restore() { this.#restore(); }

  // Used by the mock function itself.
  _implementationFor(index) {
    if (this.#onceImplementations.has(index)) {
      const once = this.#onceImplementations.get(index);
      this.#onceImplementations.delete(index);
      return once;
    }
    if (typeof this.#times === 'number' && index >= this.#times) return null;
    return this.#implementation;
  }

  _record(entry) { this.#calls.push(entry); }
}

function makeMockFunction(original, implementation, options, restore) {
  const times = options?.times;
  const context = new MockFunctionContext(implementation, restore ?? (() => {}), times);
  const mocked = function mocked(...args) {
    const index = context.callCount();
    const chosen = context._implementationFor(index) ?? original;
    const entry = { arguments: args, this: this, target: new.target, error: undefined, result: undefined };
    try {
      const result = new.target
        ? Reflect.construct(chosen, args, new.target)
        : Reflect.apply(chosen ?? (() => {}), this, args);
      entry.result = result;
      return result;
    } catch (error) {
      entry.error = error;
      throw error;
    } finally {
      context._record(entry);
    }
  };
  Object.defineProperty(mocked, 'mock', { value: context, enumerable: false, configurable: true });
  if (original !== undefined && original !== null) {
    Object.defineProperty(mocked, 'wrappedMethod', {
      value: original, enumerable: false, configurable: true, writable: true,
    });
  }
  return mocked;
}

class MockTracker {
  #mocks = [];

  fn(original, implementation, options) {
    // `fn(impl)`, `fn(original, impl)` and `fn(original, impl, options)`.
    if (typeof original === 'object' && original !== null) {
      options = original;
      original = undefined;
    } else if (typeof implementation === 'object' && implementation !== null) {
      options = implementation;
      implementation = undefined;
    }
    const impl = implementation ?? original;
    const mocked = makeMockFunction(impl, impl, options, () => {});
    this.#mocks.push(mocked);
    return mocked;
  }

  method(target, name, implementation, options = {}) {
    if (typeof target !== 'object' && typeof target !== 'function') {
      const error = new TypeError('The "object" argument must be of type object.');
      error.code = 'ERR_INVALID_ARG_TYPE';
      throw error;
    }
    const descriptor = Object.getOwnPropertyDescriptor(target, name);
    const accessor = options.getter ? 'get' : (options.setter ? 'set' : null);
    const original = accessor ? descriptor?.[accessor] : target[name];
    const impl = typeof implementation === 'function' ? implementation : original;
    const restore = () => {
      if (descriptor === undefined) {
        delete target[name];
      } else {
        Object.defineProperty(target, name, descriptor);
      }
    };
    const mocked = makeMockFunction(original, impl, options, restore);
    if (accessor) {
      Object.defineProperty(target, name, {
        ...(descriptor ?? { configurable: true, enumerable: true }),
        [accessor]: mocked,
      });
    } else {
      Object.defineProperty(target, name, {
        value: mocked,
        writable: descriptor?.writable ?? true,
        enumerable: descriptor?.enumerable ?? true,
        configurable: true,
      });
    }
    this.#mocks.push(mocked);
    return mocked;
  }

  getter(target, name, implementation, options = {}) {
    return this.method(target, name, implementation, { ...options, getter: true });
  }

  setter(target, name, implementation, options = {}) {
    return this.method(target, name, implementation, { ...options, setter: true });
  }

  reset() {
    this.restoreAll();
    this.#mocks = [];
  }

  restoreAll() {
    for (const mocked of this.#mocks) mocked.mock.restore();
  }

  timers = { enable() {}, reset() {}, tick() {} };
}

// The per-test context: assertions, lifecycle hooks and its own tracker.
function makeContext(name) {
  const t = {
    name,
    assert,
    diagnostic() {},
    skip() {},
    todo() {},
    runOnly() {},
    plan() {},
    before() {},
    after() {},
    beforeEach() {},
    afterEach() {},
    mock: new MockTracker(),
    test: subtest,
    it: subtest,
  };
  // Mirror the assert methods directly onto the context (older tests call
  // `t.strictEqual`, `t.throws`, … without going through `t.assert`).
  for (const key of Object.keys(assert)) {
    if (typeof assert[key] === 'function' && !(key in t)) {
      t[key] = assert[key].bind(assert);
    }
  }
  return t;
}

function normalize(args) {
  // (name?, options?, fn?) in any of Node's accepted orders.
  let name;
  let fn;
  for (const a of args) {
    if (typeof a === 'string') name = a;
    else if (typeof a === 'function') fn = a;
    // options object is ignored
  }
  return { name: name || (fn && fn.name) || '<anonymous>', fn };
}

function runOne(name, fn) {
  if (typeof fn !== 'function') return undefined; // pending/todo with no body
  const ctx = makeContext(name);
  try {
    // A body declared as `(t, done)` completes through the callback; the
    // returned promise keeps `await test(...)` callers working.
    if (fn.length > 1) {
      return new Promise((resolve) => {
        const done = (err) => {
          if (err != null) fail(name, err);
          resolve(undefined);
        };
        try {
          fn(ctx, done);
        } catch (err) {
          fail(name, err);
          resolve(undefined);
        }
      });
    }
    const result = fn(ctx);
    if (isThenable(result)) {
      return result.then(
        () => undefined,
        (err) => fail(name, err),
      );
    }
  } catch (err) {
    fail(name, err);
  }
  return undefined;
}

function subtest(...args) {
  const { name, fn } = normalize(args);
  return runOne(name, fn);
}

// `describe`/`suite` group sub-tests; the body registers them by calling
// `test`/`it`, which run inline, so we just invoke the body.
function describe(...args) {
  const { name, fn } = normalize(args);
  if (typeof fn === 'function') {
    const ctx = makeContext(name);
    try {
      const r = fn(ctx);
      if (isThenable(r)) return r.catch((err) => fail(name, err));
    } catch (err) {
      fail(name, err);
    }
  }
  return undefined;
}

function test(...args) {
  return subtest(...args);
}

// Variants — `.skip`/`.todo` register but do not run (counted as pass);
// `.only` runs.
test.skip = function skip() {};
test.todo = function todo() {};
test.only = function only(...args) { return subtest(...args); };
test.test = test;
test.it = test;
test.describe = describe;
test.suite = describe;

const it = test;
it.skip = test.skip;
it.todo = test.todo;
it.only = test.only;

describe.skip = function skip() {};
describe.todo = function todo() {};
describe.only = describe;

// Top-level lifecycle hooks — best-effort no-ops (suite-scoped hooks are run
// by the body in this flat model).
function noop() {}

module.exports = test;
module.exports.test = test;
module.exports.it = it;
module.exports.describe = describe;
module.exports.suite = describe;
module.exports.before = noop;
module.exports.after = noop;
module.exports.beforeEach = noop;
module.exports.afterEach = noop;
module.exports.mock = new MockTracker();
module.exports.default = test;
