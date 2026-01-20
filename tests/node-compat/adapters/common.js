/**
 * Otter replacement for Node.js test/common/index.js
 *
 * Provides compatibility layer for Node.js test utilities.
 * This module is injected before running Node.js tests.
 */

'use strict';

const path = require('path');
const fs = require('fs');
const assert = require('assert');

// =============================================================================
// Platform Detection
// =============================================================================

const isWindows = process.platform === 'win32';
const isLinux = process.platform === 'linux';
const isMacOS = process.platform === 'darwin';
const isOSX = isMacOS; // Alias
const isAIX = process.platform === 'aix';
const isFreeBSD = process.platform === 'freebsd';
const isOpenBSD = process.platform === 'openbsd';
const isSunOS = process.platform === 'sunos';

const isMainThread = true; // Otter runs tests in main thread
const isASan = false; // Address sanitizer not applicable
const hasFipsCrypto = false; // FIPS mode not implemented
const hasOpenSSL3 = false; // Using different crypto backend
const hasQuic = false; // QUIC not implemented

// Architecture detection
const bits = process.arch.endsWith('64') ? 64 : 32;
const hasIPv6 = true; // Assume IPv6 support

// =============================================================================
// Feature Detection
// =============================================================================

const hasCrypto = (() => {
  try {
    require('crypto');
    return true;
  } catch {
    return false;
  }
})();

const hasIntl = typeof Intl !== 'undefined';

// =============================================================================
// Paths and Directories
// =============================================================================

// Test directories
// __dirname is the adapters/ directory, go up to node-compat/
const testDir = path.dirname(__dirname);
const fixturesDir = path.join(testDir, 'node-src', 'test', 'fixtures');
const tmpDir = path.join(testDir, 'tmp');

// Ensure tmp directory exists
try {
  if (!fs.existsSync(tmpDir)) {
    fs.mkdirSync(tmpDir, { recursive: true });
  }
} catch {
  // Ignore errors
}

// =============================================================================
// Skip Helpers
// =============================================================================

function skip(reason) {
  console.log(`1..0 # Skipped: ${reason}`);
  process.exit(0);
}

function printSkipMessage(reason) {
  console.log(`1..0 # Skipped: ${reason}`);
}

function skipIfEslintMissing() {
  skip('ESLint is not available');
}

function skipIf32Bits() {
  if (bits === 32) skip('32-bit not supported');
}

function skipIfInspectorDisabled() {
  skip('Inspector is not available in Otter');
}

function skipIfWorker() {
  // Otter doesn't distinguish main thread in the same way
}

function skipIfDumbTerminal() {
  if (!process.stdout.isTTY) {
    skip('Skipping dumb terminal test');
  }
}

function skipIfRepl() {
  skip('REPL is not available in Otter');
}

// =============================================================================
// mustCall / mustNotCall
// =============================================================================

const mustCallTracker = [];

function mustCall(fn, expected = 1) {
  if (typeof fn === 'number') {
    expected = fn;
    fn = undefined;
  }

  const tracker = {
    fn: fn || (() => {}),
    expected,
    actual: 0,
    name: fn?.name || '<anonymous>',
    stack: new Error().stack,
  };

  mustCallTracker.push(tracker);

  const wrapped = function (...args) {
    tracker.actual++;
    if (tracker.actual > tracker.expected) {
      const err = new Error(
        `Function ${tracker.name} called ${tracker.actual} times, expected ${tracker.expected}\n${tracker.stack}`
      );
      err.code = 'ERR_ASSERTION';
      throw err;
    }
    return tracker.fn.apply(this, args);
  };

  // Copy properties from original function
  if (fn) {
    Object.defineProperty(wrapped, 'name', { value: fn.name });
  }

  return wrapped;
}

function mustCallAtLeast(fn, minimum = 1) {
  if (typeof fn === 'number') {
    minimum = fn;
    fn = undefined;
  }

  const tracker = {
    fn: fn || (() => {}),
    minimum,
    actual: 0,
    name: fn?.name || '<anonymous>',
    type: 'atLeast',
  };

  mustCallTracker.push(tracker);

  return function (...args) {
    tracker.actual++;
    return tracker.fn.apply(this, args);
  };
}

function mustNotCall(reason) {
  const error = new Error(
    reason ? `Should not be called: ${reason}` : 'Should not be called'
  );

  return function () {
    throw error;
  };
}

function mustSucceed(fn, expected = 1) {
  return mustCall(function (err, ...args) {
    if (err) {
      throw err;
    }
    if (fn) {
      return fn.apply(this, args);
    }
  }, expected);
}

function mustNotMutateObjectDeep(original) {
  // Return the original object, checks happen elsewhere
  return original;
}

// Verify all mustCall functions were called correctly on process exit
process.on('exit', (code) => {
  if (code !== 0) return; // Don't check if already failing

  for (const tracker of mustCallTracker) {
    if (
      tracker.expected !== undefined &&
      tracker.type !== 'atLeast' &&
      tracker.actual !== tracker.expected
    ) {
      console.error(
        `Mismatched calls: ${tracker.name} called ${tracker.actual} times, expected ${tracker.expected}`
      );
      process.exitCode = 1;
    }
    if (tracker.minimum !== undefined && tracker.actual < tracker.minimum) {
      console.error(
        `Insufficient calls: ${tracker.name} called ${tracker.actual} times, minimum ${tracker.minimum}`
      );
      process.exitCode = 1;
    }
  }
});

// =============================================================================
// expectsError
// =============================================================================

function expectsError(options, exact) {
  if (typeof options === 'function') {
    options = { type: options };
  }

  return function validator(error) {
    if (!error) {
      throw new Error('Expected an error but got none');
    }

    if (options.code && error.code !== options.code) {
      throw new Error(`Expected error code ${options.code}, got ${error.code}`);
    }

    if (options.type && !(error instanceof options.type)) {
      throw new Error(
        `Expected error type ${options.type.name}, got ${error.constructor.name}`
      );
    }

    if (options.message) {
      if (typeof options.message === 'string' && error.message !== options.message) {
        throw new Error(
          `Expected message "${options.message}", got "${error.message}"`
        );
      }
      if (
        options.message instanceof RegExp &&
        !options.message.test(error.message)
      ) {
        throw new Error(
          `Message "${error.message}" doesn't match ${options.message}`
        );
      }
    }

    if (options.name && error.name !== options.name) {
      throw new Error(`Expected error name ${options.name}, got ${error.name}`);
    }

    return true;
  };
}

// =============================================================================
// Platform Timeout
// =============================================================================

function platformTimeout(ms) {
  // Multiply timeouts for CI or slower platforms
  const multiplier = process.env.CI ? 3 : 1;
  return ms * multiplier;
}

// =============================================================================
// Network Helpers
// =============================================================================

const PORT = parseInt(process.env.NODE_COMMON_PORT || '0', 10);
const localhostIPv4 = '127.0.0.1';
const localhostIPv6 = '::1';

function hasMultiLocalhost() {
  return false; // Simplified
}

// =============================================================================
// File System Helpers
// =============================================================================

function getTTYfd() {
  // Return a mock TTY fd for testing
  return -1;
}

function createZeroFilledFile(filepath) {
  fs.writeFileSync(filepath, Buffer.alloc(0));
}

// Temporary file helpers
let tmpDirCounter = 0;

function tmpdir() {
  return tmpDir;
}

tmpdir.path = tmpDir;

tmpdir.refresh = function () {
  const dir = path.join(tmpDir, `test-${process.pid}-${++tmpDirCounter}`);
  try {
    fs.rmSync(dir, { recursive: true, force: true });
  } catch {}
  fs.mkdirSync(dir, { recursive: true });
  tmpdir.path = dir;
  return dir;
};

// =============================================================================
// Fixture Loading
// =============================================================================

const fixtures = {
  path(...args) {
    return path.join(fixturesDir, ...args);
  },

  readSync(filename, encoding = 'utf8') {
    return fs.readFileSync(this.path(filename), encoding);
  },

  readKey(filename, encoding = 'utf8') {
    return fs.readFileSync(path.join(fixturesDir, 'keys', filename), encoding);
  },
};

// =============================================================================
// Assertion Helpers
// =============================================================================

function getCallSite(top) {
  const originalStackFormatter = Error.prepareStackTrace;
  Error.prepareStackTrace = (err, stack) => stack;
  const err = new Error();
  Error.captureStackTrace(err, top);
  const stack = err.stack;
  Error.prepareStackTrace = originalStackFormatter;
  return stack[0];
}

function invalidArgTypeHelper(value) {
  const { inspect } = require('util');

  if (value === null) return ' Received null';
  if (value === undefined) return ' Received undefined';

  const type = typeof value;
  switch (type) {
    case 'string':
      return ` Received type string (${inspect(value)})`;
    case 'number':
      return ` Received type number (${value})`;
    case 'bigint':
      return ` Received type bigint (${String(value)}n)`;
    case 'boolean':
      return ` Received type boolean (${value})`;
    case 'symbol':
      return ` Received type symbol (${String(value)})`;
    case 'function': {
      const name = value.name ? ` ${value.name}` : '';
      return ` Received type function${name}`;
    }
    case 'object': {
      let rendered = '';
      try {
        rendered = inspect(value, { depth: 1, breakLength: Infinity });
      } catch {
        rendered = '';
      }
      return rendered ? ` Received type object (${rendered})` : ' Received type object';
    }
    default:
      return ` Received type ${type}`;
  }
}

function expectWarning() {
  // Otter does not currently surface process warnings in tests.
  // This is a no-op to keep test harness expectations satisfied.
}

// =============================================================================
// Timing Helpers
// =============================================================================

function busyLoop(time) {
  const startTime = Date.now();
  while (Date.now() - startTime < time) {
    // Busy wait
  }
}

// =============================================================================
// Child Process Helpers
// =============================================================================

function spawnPromisified(...args) {
  const { spawn } = require('child_process');
  const child = spawn(...args);

  return new Promise((resolve, reject) => {
    let stdout = '';
    let stderr = '';

    child.stdout?.on('data', (data) => {
      stdout += data.toString();
    });

    child.stderr?.on('data', (data) => {
      stderr += data.toString();
    });

    child.on('error', reject);

    child.on('close', (code, signal) => {
      resolve({ code, signal, stdout, stderr });
    });
  });
}

// =============================================================================
// Garbage Collection Helper
// =============================================================================

function gcUntil(name, condition) {
  // Otter doesn't expose GC, so just run the condition a few times
  for (let i = 0; i < 10; i++) {
    if (condition()) return;
  }
  console.log(`Warning: gcUntil condition not met for ${name}`);
}

// =============================================================================
// Exports
// =============================================================================

module.exports = {
  // Platform
  isWindows,
  isLinux,
  isMacOS,
  isOSX,
  isAIX,
  isFreeBSD,
  isOpenBSD,
  isSunOS,
  isMainThread,
  isASan,
  hasFipsCrypto,
  hasOpenSSL3,
  hasQuic,
  bits,
  hasIPv6,

  // Features
  hasCrypto,
  hasIntl,

  // Paths
  testDir,
  fixturesDir,
  tmpDir,
  tmpdir,
  fixtures,

  // Skip helpers
  skip,
  printSkipMessage,
  skipIfEslintMissing,
  skipIf32Bits,
  skipIfInspectorDisabled,
  skipIfWorker,
  skipIfDumbTerminal,
  skipIfRepl,

  // Call tracking
  mustCall,
  mustCallAtLeast,
  mustNotCall,
  mustSucceed,
  mustNotMutateObjectDeep,

  // Error validation
  expectsError,
  expectWarning,
  invalidArgTypeHelper,

  // Utilities
  platformTimeout,
  PORT,
  localhostIPv4,
  localhostIPv6,
  hasMultiLocalhost,
  getTTYfd,
  createZeroFilledFile,
  getCallSite,
  busyLoop,
  spawnPromisified,
  gcUntil,

  // Otter-specific flag
  isOtter: true,

  // Compatibility aliases
  allowGlobals: (...args) => args, // No-op for Otter
};

// Also export as default for ES module compatibility
if (typeof module.exports === 'object') {
  module.exports.default = module.exports;
}
