'use strict';
// `node:timers/promises` — promise-based timers over the global timer API.

function aborted(signal) {
  const reason = signal && signal.reason;
  const err = reason instanceof Error ? reason : new Error('The operation was aborted');
  if (!(reason instanceof Error)) err.name = 'AbortError';
  return err;
}

function setTimeout(delay = 1, value, options = {}) {
  const signal = options.signal;
  return new Promise((resolve, reject) => {
    if (signal && signal.aborted) return reject(aborted(signal));
    const t = globalThis.setTimeout(() => {
      if (signal && typeof signal.removeEventListener === 'function') signal.removeEventListener('abort', onAbort);
      resolve(value);
    }, delay);
    function onAbort() {
      globalThis.clearTimeout(t);
      reject(aborted(signal));
    }
    if (signal && typeof signal.addEventListener === 'function') signal.addEventListener('abort', onAbort, { once: true });
  });
}

function setImmediate(value, options = {}) {
  const signal = options.signal;
  return new Promise((resolve, reject) => {
    if (signal && signal.aborted) return reject(aborted(signal));
    globalThis.setImmediate(() => resolve(value));
  });
}

async function* setInterval(delay = 1, value, options = {}) {
  const signal = options.signal;
  while (true) {
    if (signal && signal.aborted) throw aborted(signal);
    await setTimeout(delay, undefined, options);
    yield value;
  }
}

const scheduler = {
  wait(delay, options) { return setTimeout(delay, undefined, options); },
  yield() { return setImmediate(undefined); },
};

module.exports = { setTimeout, setImmediate, setInterval, scheduler };
