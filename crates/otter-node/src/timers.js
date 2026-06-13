'use strict';
// `node:timers` — thin wrappers over the global timer functions plus the
// Node-specific helpers used by the test suite.

function noop() {}

const exportsObj = {
  setTimeout: (cb, delay, ...args) => setTimeout(cb, delay, ...args),
  clearTimeout: (t) => clearTimeout(t),
  setInterval: (cb, delay, ...args) => setInterval(cb, delay, ...args),
  clearInterval: (t) => clearInterval(t),
  setImmediate: (cb, ...args) => setImmediate(cb, ...args),
  clearImmediate: (t) => clearImmediate(t),
  // `active`/`unenroll`/`enroll` are legacy no-ops.
  active: noop,
  unenroll: noop,
  enroll: noop,
};
exportsObj.promises = undefined; // populated lazily by require('timers/promises')

module.exports = exportsObj;
