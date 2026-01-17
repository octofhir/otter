/**
 * Node.js assert module implementation for Otter.
 *
 * Provides assertion utilities for testing and validation.
 */
(function (global) {
  "use strict";

  /**
   * AssertionError class.
   */
  class AssertionError extends Error {
    constructor(options) {
      const {
        message,
        actual,
        expected,
        operator,
        stackStartFn,
        details,
      } = typeof options === "string" ? { message: options } : options || {};

      let msg = message;
      if (!msg) {
        if (operator === "fail") {
          msg = "Assertion failed";
        } else if (operator) {
          const actualStr = inspect(actual);
          const expectedStr = inspect(expected);
          msg = `${actualStr} ${operator} ${expectedStr}`;
        } else {
          msg = "Assertion failed";
        }
      }

      super(msg);
      this.name = "AssertionError";
      this.code = "ERR_ASSERTION";
      this.actual = actual;
      this.expected = expected;
      this.operator = operator;
      this.generatedMessage = !message;
      this.details = details;

      if (Error.captureStackTrace && stackStartFn) {
        Error.captureStackTrace(this, stackStartFn);
      }
    }

    toString() {
      return `${this.name} [${this.code}]: ${this.message}`;
    }

    toJSON() {
      return {
        name: this.name,
        code: this.code,
        message: this.message,
        actual: this.actual,
        expected: this.expected,
        operator: this.operator,
      };
    }
  }

  /**
   * Simple inspect function for error messages.
   */
  function inspect(value) {
    if (value === null) return "null";
    if (value === undefined) return "undefined";
    if (typeof value === "string") return JSON.stringify(value);
    if (typeof value === "number" || typeof value === "boolean") {
      return String(value);
    }
    if (typeof value === "bigint") return `${value}n`;
    if (typeof value === "symbol") return value.toString();
    if (typeof value === "function") {
      return value.name ? `[Function: ${value.name}]` : "[Function (anonymous)]";
    }
    if (Array.isArray(value)) {
      if (value.length === 0) return "[]";
      if (value.length <= 5) {
        return `[ ${value.map(inspect).join(", ")} ]`;
      }
      return `[ ${value.slice(0, 5).map(inspect).join(", ")}, ... ${value.length - 5} more items ]`;
    }
    if (value instanceof Error) {
      return `${value.name}: ${value.message}`;
    }
    if (value instanceof RegExp) {
      return value.toString();
    }
    if (value instanceof Date) {
      return value.toISOString();
    }
    if (typeof value === "object") {
      const keys = Object.keys(value);
      if (keys.length === 0) return "{}";
      if (keys.length <= 3) {
        const pairs = keys.map((k) => `${k}: ${inspect(value[k])}`);
        return `{ ${pairs.join(", ")} }`;
      }
      const pairs = keys.slice(0, 3).map((k) => `${k}: ${inspect(value[k])}`);
      return `{ ${pairs.join(", ")}, ... ${keys.length - 3} more props }`;
    }
    return String(value);
  }

  /**
   * Deep equality check.
   */
  function isDeepEqual(a, b, strict = false) {
    // Same reference or primitives
    if (a === b) return true;

    // Handle null/undefined
    if (a == null || b == null) {
      return strict ? a === b : a == b;
    }

    // Type check for strict mode
    if (strict && typeof a !== typeof b) return false;

    // Non-strict equality for primitives
    if (!strict && a == b) return true;

    // Arrays
    if (Array.isArray(a) && Array.isArray(b)) {
      if (a.length !== b.length) return false;
      for (let i = 0; i < a.length; i++) {
        if (!isDeepEqual(a[i], b[i], strict)) return false;
      }
      return true;
    }

    // One is array, other is not
    if (Array.isArray(a) !== Array.isArray(b)) return false;

    // Date
    if (a instanceof Date && b instanceof Date) {
      return a.getTime() === b.getTime();
    }

    // RegExp
    if (a instanceof RegExp && b instanceof RegExp) {
      return a.toString() === b.toString();
    }

    // Objects
    if (typeof a === "object" && typeof b === "object") {
      const keysA = Object.keys(a);
      const keysB = Object.keys(b);

      if (keysA.length !== keysB.length) return false;

      for (const key of keysA) {
        if (!Object.prototype.hasOwnProperty.call(b, key)) return false;
        if (!isDeepEqual(a[key], b[key], strict)) return false;
      }

      return true;
    }

    return false;
  }

  /**
   * Check if error matches expected.
   */
  function matchError(actual, expected) {
    if (!expected) return true;

    // Expected is a constructor
    if (typeof expected === "function") {
      // Check if it's an Error constructor
      if (expected.prototype instanceof Error || expected === Error) {
        return actual instanceof expected;
      }
      // Validator function
      return expected(actual) === true;
    }

    // Expected is a RegExp
    if (expected instanceof RegExp) {
      return expected.test(actual.message || String(actual));
    }

    // Expected is an Error instance
    if (expected instanceof Error) {
      return actual.message === expected.message;
    }

    // Expected is an object with properties to match
    if (typeof expected === "object" && expected !== null) {
      for (const key of Object.keys(expected)) {
        if (!isDeepEqual(actual[key], expected[key], true)) {
          return false;
        }
      }
      return true;
    }

    return false;
  }

  // ==========================================================================
  // Assert functions
  // ==========================================================================

  /**
   * Assert that value is truthy.
   */
  function assert(value, message) {
    if (!value) {
      throw new AssertionError({
        message: message || "The expression evaluated to a falsy value",
        actual: value,
        expected: true,
        operator: "==",
        stackStartFn: assert,
      });
    }
  }

  /**
   * Assert that value is truthy (alias for assert).
   */
  function ok(value, message) {
    if (!value) {
      throw new AssertionError({
        message: message || "The expression evaluated to a falsy value",
        actual: value,
        expected: true,
        operator: "==",
        stackStartFn: ok,
      });
    }
  }

  /**
   * Assert loose equality (==).
   */
  function equal(actual, expected, message) {
    /* eslint-disable eqeqeq */
    if (actual != expected) {
      throw new AssertionError({
        message,
        actual,
        expected,
        operator: "==",
        stackStartFn: equal,
      });
    }
  }

  /**
   * Assert loose inequality (!=).
   */
  function notEqual(actual, expected, message) {
    /* eslint-disable eqeqeq */
    if (actual == expected) {
      throw new AssertionError({
        message,
        actual,
        expected,
        operator: "!=",
        stackStartFn: notEqual,
      });
    }
  }

  /**
   * Assert strict equality (===).
   */
  function strictEqual(actual, expected, message) {
    if (actual !== expected) {
      throw new AssertionError({
        message,
        actual,
        expected,
        operator: "===",
        stackStartFn: strictEqual,
      });
    }
  }

  /**
   * Assert strict inequality (!==).
   */
  function notStrictEqual(actual, expected, message) {
    if (actual === expected) {
      throw new AssertionError({
        message,
        actual,
        expected,
        operator: "!==",
        stackStartFn: notStrictEqual,
      });
    }
  }

  /**
   * Assert deep equality.
   */
  function deepEqual(actual, expected, message) {
    if (!isDeepEqual(actual, expected, false)) {
      throw new AssertionError({
        message,
        actual,
        expected,
        operator: "deepEqual",
        stackStartFn: deepEqual,
      });
    }
  }

  /**
   * Assert deep inequality.
   */
  function notDeepEqual(actual, expected, message) {
    if (isDeepEqual(actual, expected, false)) {
      throw new AssertionError({
        message,
        actual,
        expected,
        operator: "notDeepEqual",
        stackStartFn: notDeepEqual,
      });
    }
  }

  /**
   * Assert deep strict equality.
   */
  function deepStrictEqual(actual, expected, message) {
    if (!isDeepEqual(actual, expected, true)) {
      throw new AssertionError({
        message,
        actual,
        expected,
        operator: "deepStrictEqual",
        stackStartFn: deepStrictEqual,
      });
    }
  }

  /**
   * Assert deep strict inequality.
   */
  function notDeepStrictEqual(actual, expected, message) {
    if (isDeepEqual(actual, expected, true)) {
      throw new AssertionError({
        message,
        actual,
        expected,
        operator: "notDeepStrictEqual",
        stackStartFn: notDeepStrictEqual,
      });
    }
  }

  /**
   * Assert that function throws.
   */
  function throws(fn, error, message) {
    if (typeof fn !== "function") {
      throw new TypeError("First argument must be a function");
    }

    // Handle argument variations
    if (typeof error === "string") {
      message = error;
      error = undefined;
    }

    let threw = false;
    let actual;

    try {
      fn();
    } catch (e) {
      threw = true;
      actual = e;
    }

    if (!threw) {
      throw new AssertionError({
        message: message || "Missing expected exception",
        actual: undefined,
        expected: error,
        operator: "throws",
        stackStartFn: throws,
      });
    }

    if (error && !matchError(actual, error)) {
      throw new AssertionError({
        message: message || "Thrown error did not match expected",
        actual,
        expected: error,
        operator: "throws",
        stackStartFn: throws,
      });
    }
  }

  /**
   * Assert that function does not throw.
   */
  function doesNotThrow(fn, error, message) {
    if (typeof fn !== "function") {
      throw new TypeError("First argument must be a function");
    }

    // Handle argument variations
    if (typeof error === "string") {
      message = error;
      error = undefined;
    }

    try {
      fn();
    } catch (actual) {
      if (!error || matchError(actual, error)) {
        throw new AssertionError({
          message: message || `Got unwanted exception: ${actual.message || actual}`,
          actual,
          expected: error,
          operator: "doesNotThrow",
          stackStartFn: doesNotThrow,
        });
      }
      // Re-throw if error doesn't match the expected pattern
      throw actual;
    }
  }

  /**
   * Assert that promise rejects.
   */
  async function rejects(asyncFn, error, message) {
    // Handle argument variations
    if (typeof error === "string") {
      message = error;
      error = undefined;
    }

    let promise;
    if (typeof asyncFn === "function") {
      promise = asyncFn();
    } else {
      promise = asyncFn;
    }

    let threw = false;
    let actual;

    try {
      await promise;
    } catch (e) {
      threw = true;
      actual = e;
    }

    if (!threw) {
      throw new AssertionError({
        message: message || "Missing expected rejection",
        actual: undefined,
        expected: error,
        operator: "rejects",
        stackStartFn: rejects,
      });
    }

    if (error && !matchError(actual, error)) {
      throw new AssertionError({
        message: message || "Rejected value did not match expected",
        actual,
        expected: error,
        operator: "rejects",
        stackStartFn: rejects,
      });
    }
  }

  /**
   * Assert that promise does not reject.
   */
  async function doesNotReject(asyncFn, error, message) {
    // Handle argument variations
    if (typeof error === "string") {
      message = error;
      error = undefined;
    }

    let promise;
    if (typeof asyncFn === "function") {
      promise = asyncFn();
    } else {
      promise = asyncFn;
    }

    try {
      await promise;
    } catch (actual) {
      if (!error || matchError(actual, error)) {
        throw new AssertionError({
          message: message || `Got unwanted rejection: ${actual.message || actual}`,
          actual,
          expected: error,
          operator: "doesNotReject",
          stackStartFn: doesNotReject,
        });
      }
      throw actual;
    }
  }

  /**
   * Throw if value is truthy.
   */
  function ifError(value) {
    if (value != null) {
      let error = value;
      if (typeof value === "string") {
        error = new Error(value);
      } else if (!(value instanceof Error)) {
        error = new AssertionError({
          message: `ifError got unwanted value: ${inspect(value)}`,
          actual: value,
          operator: "ifError",
          stackStartFn: ifError,
        });
      }
      throw error;
    }
  }

  /**
   * Always fail with message.
   */
  function fail(actual, expected, message, operator) {
    // Handle single argument case (just message)
    if (arguments.length === 0) {
      message = "Failed";
    } else if (arguments.length === 1) {
      message = actual;
      actual = undefined;
    }

    throw new AssertionError({
      message,
      actual,
      expected,
      operator: operator || "fail",
      stackStartFn: fail,
    });
  }

  /**
   * Assert string matches regexp.
   */
  function match(string, regexp, message) {
    if (!(regexp instanceof RegExp)) {
      throw new TypeError("Second argument must be a RegExp");
    }

    if (!regexp.test(string)) {
      throw new AssertionError({
        message: message || `The input did not match the regular expression ${regexp}`,
        actual: string,
        expected: regexp,
        operator: "match",
        stackStartFn: match,
      });
    }
  }

  /**
   * Assert string does not match regexp.
   */
  function doesNotMatch(string, regexp, message) {
    if (!(regexp instanceof RegExp)) {
      throw new TypeError("Second argument must be a RegExp");
    }

    if (regexp.test(string)) {
      throw new AssertionError({
        message: message || `The input was expected to not match the regular expression ${regexp}`,
        actual: string,
        expected: regexp,
        operator: "doesNotMatch",
        stackStartFn: doesNotMatch,
      });
    }
  }

  // ==========================================================================
  // Strict mode (assert.strict)
  // ==========================================================================

  const strict = function strictAssert(value, message) {
    if (!value) {
      throw new AssertionError({
        message: message || "The expression evaluated to a falsy value",
        actual: value,
        expected: true,
        operator: "==",
        stackStartFn: strictAssert,
      });
    }
  };

  // Add all methods to strict
  strict.ok = ok;
  strict.equal = strictEqual;
  strict.notEqual = notStrictEqual;
  strict.strictEqual = strictEqual;
  strict.notStrictEqual = notStrictEqual;
  strict.deepEqual = deepStrictEqual;
  strict.notDeepEqual = notDeepStrictEqual;
  strict.deepStrictEqual = deepStrictEqual;
  strict.notDeepStrictEqual = notDeepStrictEqual;
  strict.throws = throws;
  strict.doesNotThrow = doesNotThrow;
  strict.rejects = rejects;
  strict.doesNotReject = doesNotReject;
  strict.ifError = ifError;
  strict.fail = fail;
  strict.match = match;
  strict.doesNotMatch = doesNotMatch;
  strict.AssertionError = AssertionError;
  strict.strict = strict;

  // ==========================================================================
  // Module exports
  // ==========================================================================

  // Main assert function with methods
  assert.ok = ok;
  assert.equal = equal;
  assert.notEqual = notEqual;
  assert.strictEqual = strictEqual;
  assert.notStrictEqual = notStrictEqual;
  assert.deepEqual = deepEqual;
  assert.notDeepEqual = notDeepEqual;
  assert.deepStrictEqual = deepStrictEqual;
  assert.notDeepStrictEqual = notDeepStrictEqual;
  assert.throws = throws;
  assert.doesNotThrow = doesNotThrow;
  assert.rejects = rejects;
  assert.doesNotReject = doesNotReject;
  assert.ifError = ifError;
  assert.fail = fail;
  assert.match = match;
  assert.doesNotMatch = doesNotMatch;
  assert.AssertionError = AssertionError;
  assert.strict = strict;

  // Register as node:assert module
  if (typeof __registerModule === "function") {
    __registerModule("assert", assert);
  }

  // Also expose on global for direct access
  global.__otter_assert = assert;
  global.AssertionError = AssertionError;
})(globalThis);
