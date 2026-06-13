'use strict';
// internal/util — the subset of Node's lib/internal/util the conformance suite
// reaches under --expose-internals. Kept intentionally small; grows as tests
// need more.

function argTypeError(name, msg, value) {
  let received;
  if (value === null || value === undefined) received = ` Received ${value}`;
  else if (typeof value === 'object') {
    received = ` Received an instance of ${value.constructor ? value.constructor.name : 'Object'}`;
  } else if (typeof value === 'string') received = ` Received type string ('${value}')`;
  else received = ` Received type ${typeof value} (${String(value)})`;
  const e = new TypeError(`The "${name}" ${msg}.${received}`);
  e.code = 'ERR_INVALID_ARG_TYPE';
  return e;
}
function rangeError(name, range, value) {
  const e = new RangeError(
    `The value of "${name}" is out of range. It must be ${range}. Received ${value}`
  );
  e.code = 'ERR_OUT_OF_RANGE';
  return e;
}

// §internalBinding('util').sleep — validate a uint32 millisecond count, then
// busy-wait. Tests exercise the validation path (the actual blocking is a
// busy loop bounded by the requested duration).
function sleep(msec) {
  if (typeof msec !== 'number') {
    throw argTypeError('msec', 'argument must be of type number', msec);
  }
  if (!Number.isInteger(msec) || msec < 0 || msec > 0xffffffff) {
    throw rangeError('msec', '>= 0 && <= 4294967295', msec);
  }
  const end = Date.now() + msec;
  while (Date.now() < end) { /* busy wait */ }
}

// §emitExperimentalWarning — warn once per feature via process warnings.
const experimentalWarnings = new Set();
function emitExperimentalWarning(feature) {
  if (experimentalWarnings.has(feature)) return;
  experimentalWarnings.add(feature);
  const msg = `${feature} is an experimental feature and might change at any time`;
  if (typeof process !== 'undefined' && typeof process.emitWarning === 'function') {
    process.emitWarning(msg, 'ExperimentalWarning');
  }
}

// §deprecate — mirrors util.deprecate (one warning per wrapped function).
function deprecate(fn, msg, code) {
  let warned = false;
  function deprecated(...args) {
    if (!warned) {
      warned = true;
      if (typeof process !== 'undefined' && typeof process.emitWarning === 'function') {
        process.emitWarning(msg, 'DeprecationWarning', code);
      }
    }
    return fn.apply(this, args);
  }
  return deprecated;
}

const kEmptyObject = Object.freeze(Object.create(null));

module.exports = {
  sleep,
  emitExperimentalWarning,
  deprecate,
  kEmptyObject,
};
