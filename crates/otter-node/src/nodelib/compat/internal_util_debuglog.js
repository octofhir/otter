'use strict';
// NODE_DEBUG-gated section loggers. The optimization callback receives the
// resolved logger on FIRST USE, never synchronously — callers assign the
// result to the same `let` binding the callback writes, so an eager call
// lands in its temporal dead zone. `enabled` is a getter on every handed-out
// logger.

// NODE_DEBUG is a comma-separated list of section patterns where `*` is a
// wildcard; every other character matches literally (sections like `###`
// or `hi:)` are legal).
function sectionEnabled(set) {
  for (const part of String(process.env.NODE_DEBUG ?? '').split(',')) {
    const trimmed = part.trim();
    if (trimmed === '') continue;
    const pattern = new RegExp(
      `^${trimmed.replace(/[|\\{}()[\]^$+?.]/g, '\\$&').replace(/\*/g, '.*')}$`,
      'i',
    );
    if (pattern.test(set)) return true;
  }
  return false;
}

function debuglogImpl(enabled, set) {
  if (!enabled) return function debug() {};
  const pid = process.pid;
  return function debug(...args) {
    const colors = process.stderr?.hasColors?.() === true;
    const msg = require('util').formatWithOptions({ colors }, ...args);
    const shownPid = colors ? `\u001b[33m${pid}\u001b[39m` : pid;
    process.stderr?.write?.(`${set} ${shownPid}: ${msg}\n`);
  };
}

function debuglog(set, cb) {
  const section = String(set).toUpperCase();
  let impl;
  let enabled;
  // NODE_DEBUG is read once, at first use; the callback then receives the
  // resolved logger itself, so a caller that rebinds to it calls the no-op
  // directly when the section is off.
  function init() {
    if (impl === undefined) {
      enabled = sectionEnabled(section);
      impl = debuglogImpl(enabled, section);
      if (typeof cb === 'function') {
        const callback = cb;
        cb = undefined;
        callback(impl);
      }
    }
    return impl;
  }
  function logger(...args) {
    return init()(...args);
  }
  Object.defineProperty(logger, 'enabled', {
    get() {
      init();
      return enabled;
    },
    configurable: true,
    enumerable: true,
  });
  return logger;
}

module.exports = { debuglog, debuglogImpl };
