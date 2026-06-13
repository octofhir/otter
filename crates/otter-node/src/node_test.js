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

// A no-op async-aware mock/diagnostic surface; expanded as tests demand.
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
    mock: {
      fn(impl) { return impl || (() => {}); },
      method() {},
      reset() {},
      restoreAll() {},
      timers: { enable() {}, reset() {}, tick() {} },
    },
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
module.exports.mock = {
  fn(impl) { return impl || (() => {}); },
  method() {},
  reset() {},
  restoreAll() {},
  timers: { enable() {}, reset() {}, tick() {} },
};
module.exports.default = test;
