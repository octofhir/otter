'use strict';
// NODE_DEBUG-gated section loggers. The optimization callback receives the
// real logger on FIRST USE, never synchronously — callers assign the result
// to the same `let` binding the callback writes, so an eager call lands in
// its temporal dead zone.
function debuglog(set, cb) {
  const enabled = new RegExp(`\\b${set}\\b`, 'i')
    .test(String(process.env.NODE_DEBUG ?? ''));
  const logger = enabled
    ? (...args) => process.stderr?.write?.(
        `${set.toUpperCase()} ${process.pid}: ${require('util').format(...args)}\n`)
    : () => {};
  logger.enabled = enabled;
  function wrapper(...args) {
    if (typeof cb === 'function') {
      cb(logger);
      cb = undefined;
    }
    return logger(...args);
  }
  wrapper.enabled = enabled;
  return wrapper;
}
module.exports = { debuglog, debuglogImpl: debuglog };
