'use strict';
function addAbortListener(signal, listener) {
  if (signal === undefined) {
    const err = new TypeError('The "signal" argument must be an instance of AbortSignal.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  if (signal.aborted) {
    queueMicrotask(() => listener());
    return { [Symbol.dispose ?? Symbol.for('nodejs.dispose')]() {} };
  }
  signal.addEventListener('abort', listener, { once: true });
  return {
    [Symbol.dispose ?? Symbol.for('nodejs.dispose')]() {
      signal.removeEventListener('abort', listener);
    },
  };
}
module.exports = { addAbortListener };
