'use strict';
// `internal/errors` — small Node-compatible error factory surface used by
// Node's own compatibility fixtures and internal shims.

const kIsNodeError = Symbol('kIsNodeError');

function valueName(value) {
  if (value === null) return 'null';
  if (Array.isArray(value)) return 'Array';
  return typeof value;
}

function makeNodeError(Base, code, messageFactory) {
  return class NodeError extends Base {
    constructor(...args) {
      super(messageFactory ? messageFactory(...args) : code);
      this.code = code;
      this[kIsNodeError] = true;
    }
  };
}

const codes = {
  ERR_INVALID_ARG_TYPE: makeNodeError(TypeError, 'ERR_INVALID_ARG_TYPE',
    (name, expected, actual) => {
      const label = name ? `The "${name}" argument` : 'The argument';
      const expect = Array.isArray(expected) ? expected.join(' or ') : expected;
      return `${label} must be of type ${expect}. Received ${valueName(actual)}`;
    }),
  ERR_INVALID_ARG_VALUE: makeNodeError(TypeError, 'ERR_INVALID_ARG_VALUE',
    (name, value) => `The argument '${name}' is invalid. Received ${value}`),
  ERR_OUT_OF_RANGE: makeNodeError(RangeError, 'ERR_OUT_OF_RANGE',
    (name = 'value', range = 'out of range', value) =>
      `The value of "${name}" is out of range. It must be ${range}. Received ${value}`),
  ERR_UNKNOWN_ENCODING: makeNodeError(TypeError, 'ERR_UNKNOWN_ENCODING',
    (encoding) => `Unknown encoding: ${encoding}`),
  ERR_BUFFER_OUT_OF_BOUNDS: makeNodeError(RangeError, 'ERR_BUFFER_OUT_OF_BOUNDS',
    (name = 'offset') => `"${name}" is outside of buffer bounds`),
};

class AbortError extends Error {
  constructor(message = 'The operation was aborted') {
    super(message);
    this.name = 'AbortError';
    this.code = 'ABORT_ERR';
  }
}

class SystemError extends Error {
  constructor(ctx = {}) {
    const syscall = ctx.syscall || 'syscall';
    const code = ctx.code || 'UNKNOWN';
    const message = ctx.message || code;
    super(`A system error occurred: ${syscall} returned ${code} (${message})`);
    this.name = 'SystemError';
    this.code = code;
    this.info = ctx;
  }
}

function E(code, message, Base = Error) {
  codes[code] = makeNodeError(Base, code, typeof message === 'function' ? message : () => String(message));
}

function hideStackFrames(fn) {
  return fn;
}

function aggregateTwoErrors(innerError, outerError) {
  if (innerError && outerError) return new AggregateError([innerError, outerError]);
  return innerError || outerError;
}

function formatList(list, type = 'and') {
  const values = Array.from(list);
  if (values.length <= 2) return values.join(` ${type} `);
  return `${values.slice(0, -1).join(', ')}, ${type} ${values[values.length - 1]}`;
}

function UVException(ctx) {
  return new SystemError(ctx);
}

function UVExceptionWithHostPort(ctx) {
  return new SystemError(ctx);
}

module.exports = {
  AbortError,
  E,
  SystemError,
  UVException,
  UVExceptionWithHostPort,
  aggregateTwoErrors,
  codes,
  formatList,
  hideStackFrames,
  kIsNodeError,
};
