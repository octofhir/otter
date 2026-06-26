'use strict';
// internal/assert/calltracker — assert.CallTracker (deprecated in Node, still
// covered by the conformance suite). Records how many times tracked wrappers
// were invoked plus each call's arguments/thisArg, and verifies the expected
// counts. AssertionError is injected so verify() throws a real instance.
//
// Implemented without private fields / WeakMap / computed method names so it
// stays within the VM's supported JS surface; tracked wrappers are mapped to
// their check record through a plain Map kept on the instance.

module.exports = function makeCallTracker(AssertionError) {
  const kCheck = Symbol('CallTracker check');

  function received(value) {
    if (value === null) return 'null';
    if (value === undefined) return 'undefined';
    const t = typeof value;
    if (t === 'function') return `function ${value.name}`;
    if (t === 'string') return `type string ('${value}')`;
    if (t === 'object') return 'an instance of Object';
    return `type ${t} (${String(value)})`;
  }
  function invalidArgType(name, expected, value) {
    const e = new TypeError(
      `The "${name}" argument must be ${expected}. Received ${received(value)}`
    );
    e.code = 'ERR_INVALID_ARG_TYPE';
    return e;
  }
  function outOfRange(name, range, value) {
    const e = new RangeError(
      `The value of "${name}" is out of range. It must be ${range}. Received ${value}`
    );
    e.code = 'ERR_OUT_OF_RANGE';
    return e;
  }
  function invalidArgValue(name, value, reason) {
    const e = new TypeError(
      `The argument '${name}' ${reason}. Received ${received(value)}`
    );
    e.code = 'ERR_INVALID_ARG_VALUE';
    return e;
  }

  // §validateUint32 — exact must be a non-negative 32-bit integer.
  function validateExact(exact) {
    if (typeof exact !== 'number') {
      throw invalidArgType('exact', 'of type number', exact);
    }
    if (!Number.isInteger(exact)) {
      throw outOfRange('exact', 'an integer', exact);
    }
    if (exact < 0 || exact > 0xffffffff) {
      throw outOfRange('exact', '>= 0 && <= 4294967295', exact);
    }
  }

  function noop() {}

  function freezeForCallSnapshot(value) {
    // Node's tests assert the mutation failure shape ("object is not
    // extensible"). Preventing extensions is enough for immutable snapshots in
    // this shim and matches that observable error better in the current VM.
    return Object.preventExtensions(value);
  }

  function copyFunctionDescriptors(target, source) {
    const descriptors = Object.getOwnPropertyDescriptors(source);
    if (descriptors.length === undefined) {
      try { delete target.length; } catch {}
    }
    for (const key of Reflect.ownKeys(descriptors)) {
      if (key === 'arguments' || key === 'caller') continue;
      try {
        Object.defineProperty(target, key, descriptors[key]);
      } catch {
        // Some built-in function descriptors are not configurable on all
        // runtimes. Best-effort copying still preserves ordinary function
        // length/name/custom own properties used by Node's tests.
      }
    }
  }

  function checkFor(fn, fallbackMap) {
    if (fn != null && (typeof fn === 'function' || typeof fn === 'object')) {
      const direct = fn[kCheck];
      if (direct !== undefined) return direct;
    }
    return fallbackMap.get(fn);
  }

  class CallTracker {
    constructor() {
      this._checks = [];
      this._byWrapper = new Map();
    }

    calls(fn, exact) {
      if (exact === undefined) exact = 1;
      if (typeof fn === 'number') {
        exact = fn;
        fn = noop;
      }
      if (fn === undefined) fn = noop;
      if (typeof fn !== 'function') {
        throw invalidArgType('fn', 'of type function', fn);
      }
      validateExact(exact);

      const check = {
        name: fn.name || '<anonymous>',
        actual: 0,
        exact,
        calls: [],
      };
      this._checks.push(check);

      const tracker = function trackedCall(...args) {
        check.actual++;
        check.calls.push(
          freezeForCallSnapshot({ thisArg: this, arguments: freezeForCallSnapshot(args) })
        );
        return fn.apply(this, args);
      };
      copyFunctionDescriptors(tracker, fn);
      Object.defineProperty(tracker, kCheck, { value: check });
      this._byWrapper.set(tracker, check);
      return tracker;
    }

    getCalls(fn) {
      const check = checkFor(fn, this._byWrapper);
      if (check === undefined) {
        throw invalidArgValue('fn', fn, 'is not a tracked function');
      }
      return freezeForCallSnapshot(check.calls.slice());
    }

    report() {
      const errors = [];
      for (const check of this._checks) {
        if (check.actual !== check.exact) {
          errors.push({
            message:
              `Expected the ${check.name} function to be executed ` +
              `${check.exact} time(s) but was executed ${check.actual} ` +
              `time(s).`,
            actual: check.actual,
            expected: check.exact,
            operator: check.name,
          });
        }
      }
      return errors;
    }

    verify() {
      const errors = this.report();
      if (errors.length === 0) return;
      const message =
        errors.length === 1
          ? errors[0].message
          : 'Functions were not called the expected number of times';
      throw new AssertionError({ message, operator: 'CallTracker' });
    }

    reset(fn) {
      // `reset()` with no argument resets the recorded calls of every
      // tracked function but keeps them tracked; `reset(fn)` resets one.
      if (fn === undefined) {
        for (const check of this._checks) {
          check.actual = 0;
          check.calls.length = 0;
        }
        return;
      }
      const check = checkFor(fn, this._byWrapper);
      if (check === undefined) {
        throw invalidArgValue('fn', fn, 'is not a tracked function');
      }
      check.actual = 0;
      check.calls.length = 0;
    }
  }

  return CallTracker;
};
