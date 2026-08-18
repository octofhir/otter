'use strict';
// internal/timers — the slices the vendored http/net stack reads: unref'd
// scheduling and duration validation.
const {
  codes: { ERR_INVALID_ARG_TYPE, ERR_OUT_OF_RANGE },
} = require('internal/errors');

function setUnrefTimeout(callback, after, ...args) {
  const timer = setTimeout(callback, after, ...args);
  if (typeof timer?.unref === 'function') timer.unref();
  return timer;
}

const TIMEOUT_MAX = 2 ** 31 - 1;

function getTimerDuration(msecs, name) {
  if (typeof msecs !== 'number') {
    throw new ERR_INVALID_ARG_TYPE(name, 'number', msecs);
  }
  if (msecs < 0 || !Number.isFinite(msecs)) {
    throw new ERR_OUT_OF_RANGE(name, 'a non-negative finite number', msecs);
  }
  if (msecs > TIMEOUT_MAX) return TIMEOUT_MAX;
  return msecs;
}

// Timer reuse on keep-alive paths: the cleared Timeout stays parked on
// the stream under `kTimeout` so the next `setTimeout` can revive it.
// The host clears timers fully, so revival is a fresh unref'd timer.
function reuseOrCreateUnrefTimeout(_existing, callback, duration, arg) {
  return setUnrefTimeout(callback, duration, arg);
}

module.exports = {
  setUnrefTimeout,
  reuseOrCreateUnrefTimeout,
  getTimerDuration,
  kTimeout: Symbol('timeout'),
  kRefed: Symbol('refed'),
};
