/**
 * Test harness for Node.js tests running on Otter
 *
 * This module sets up the global environment expected by Node.js tests.
 * It should be loaded before running any Node.js test file.
 */

'use strict';

const common = require('./common.js');
const assert = require('assert');
const path = require('path');

// =============================================================================
// Global Setup
// =============================================================================

// Make common available globally (many Node.js tests expect this)
globalThis.common = common;

// Export common utilities to global scope
globalThis.mustCall = common.mustCall;
globalThis.mustCallAtLeast = common.mustCallAtLeast;
globalThis.mustNotCall = common.mustNotCall;
globalThis.mustSucceed = common.mustSucceed;
globalThis.expectsError = common.expectsError;
globalThis.skip = common.skip;

// =============================================================================
// Assertion Extensions
// =============================================================================

// Add missing assert methods if needed
if (!assert.match) {
  assert.match = function match(string, regexp, message) {
    if (!regexp.test(string)) {
      throw new assert.AssertionError({
        message: message || `Expected ${string} to match ${regexp}`,
        actual: string,
        expected: regexp,
        operator: 'match',
      });
    }
  };
}

if (!assert.doesNotMatch) {
  assert.doesNotMatch = function doesNotMatch(string, regexp, message) {
    if (regexp.test(string)) {
      throw new assert.AssertionError({
        message: message || `Expected ${string} to not match ${regexp}`,
        actual: string,
        expected: regexp,
        operator: 'doesNotMatch',
      });
    }
  };
}

if (!assert.rejects) {
  assert.rejects = async function rejects(asyncFn, error, message) {
    let threw = false;
    let actualError;

    try {
      await (typeof asyncFn === 'function' ? asyncFn() : asyncFn);
    } catch (err) {
      threw = true;
      actualError = err;
    }

    if (!threw) {
      throw new assert.AssertionError({
        message: message || 'Expected promise to reject',
        operator: 'rejects',
      });
    }

    if (error) {
      if (typeof error === 'function') {
        if (error.prototype !== undefined && actualError instanceof error) {
          return;
        }
        if (Error.isPrototypeOf(error) && actualError instanceof error) {
          return;
        }
        const validator = error;
        if (validator(actualError) === true) {
          return;
        }
      } else if (error instanceof RegExp) {
        if (error.test(actualError.message)) {
          return;
        }
      } else if (typeof error === 'object') {
        for (const key of Object.keys(error)) {
          if (actualError[key] !== error[key]) {
            throw new assert.AssertionError({
              message: `Expected error.${key} to be ${error[key]}, got ${actualError[key]}`,
              operator: 'rejects',
            });
          }
        }
        return;
      }

      throw new assert.AssertionError({
        message: message || `Rejection did not match expected error`,
        actual: actualError,
        expected: error,
        operator: 'rejects',
      });
    }
  };
}

if (!assert.doesNotReject) {
  assert.doesNotReject = async function doesNotReject(asyncFn, error, message) {
    try {
      await (typeof asyncFn === 'function' ? asyncFn() : asyncFn);
    } catch (err) {
      throw new assert.AssertionError({
        message: message || `Expected promise to not reject, but it rejected with: ${err}`,
        actual: err,
        operator: 'doesNotReject',
      });
    }
  };
}

// =============================================================================
// Process Extensions
// =============================================================================

// Ensure process.binding exists (stub)
if (!process.binding) {
  process.binding = function binding(name) {
    throw new Error(`process.binding('${name}') is not supported in Otter`);
  };
}

// Ensure process._getActiveHandles exists (stub)
if (!process._getActiveHandles) {
  process._getActiveHandles = function _getActiveHandles() {
    return [];
  };
}

// Ensure process._getActiveRequests exists (stub)
if (!process._getActiveRequests) {
  process._getActiveRequests = function _getActiveRequests() {
    return [];
  };
}

// =============================================================================
// Console Extensions
// =============================================================================

// Ensure console.time/timeEnd exist
if (!console.time) {
  const timers = new Map();

  console.time = function time(label = 'default') {
    timers.set(label, Date.now());
  };

  console.timeEnd = function timeEnd(label = 'default') {
    const start = timers.get(label);
    if (start !== undefined) {
      console.log(`${label}: ${Date.now() - start}ms`);
      timers.delete(label);
    }
  };

  console.timeLog = function timeLog(label = 'default', ...args) {
    const start = timers.get(label);
    if (start !== undefined) {
      console.log(`${label}: ${Date.now() - start}ms`, ...args);
    }
  };
}

// =============================================================================
// Module System Patches
// =============================================================================

// Patch require to handle test/common paths
const originalRequire = globalThis.require;

if (originalRequire) {
  const patchedRequire = function patchedRequire(id) {
    // Handle '../common' style imports from test files
    if (id === '../common' || id === '../common/index' || id === '../common/index.js') {
      return common;
    }

    // Handle test/common absolute paths
    if (id.includes('test/common')) {
      return common;
    }

    // Handle fixtures
    if (id === '../common/fixtures') {
      return common.fixtures;
    }

    // Handle tmpdir
    if (id === '../common/tmpdir') {
      return common.tmpdir;
    }

    return originalRequire(id);
  };

  // Copy require properties
  Object.assign(patchedRequire, originalRequire);
  globalThis.require = patchedRequire;
}

// =============================================================================
// Error Handling
// =============================================================================

// Catch unhandled rejections and turn them into test failures
process.on('unhandledRejection', (reason, promise) => {
  console.error('Unhandled Rejection:', reason);
  process.exitCode = 1;
});

// Catch uncaught exceptions
process.on('uncaughtException', (error) => {
  console.error('Uncaught Exception:', error);
  process.exitCode = 1;
});

// =============================================================================
// Test Result Indicators
// =============================================================================

// Override exit to check for success
const originalExit = process.exit;
process.exit = function exit(code) {
  if (code === 0 || code === undefined) {
    // Test passed
  } else {
    // Test failed
  }
  return originalExit.call(process, code);
};

// =============================================================================
// Module Exports
// =============================================================================

module.exports = {
  common,
  assert,
  setup() {
    // Called at the start of each test
    common.tmpdir.refresh();
  },
};

// Signal that harness is loaded
console.log('# Otter Node.js test harness loaded');
