'use strict';
// `node:async_hooks` — asynchronous context tracking.
//
// The engine carries one context value per isolate: a microtask or timer
// captures it when it is queued and restores it around its own execution. A
// store is therefore a plain frame object, and entering one means installing a
// copy with an extra binding — the copy is what later continuations inherit.

const native = globalThis.__otterAsyncContextNative;
// The id machinery, the hook list and `AsyncResource` live in
// `internal/async_hooks`; this module is their public face plus the
// context-carrying storage built on the isolate's async context.
const {
  AsyncResource,
  createHook,
  executionAsyncId,
  triggerAsyncId,
  executionAsyncResource,
} = require('internal/async_hooks');

const kStore = Symbol('kStore');

function currentFrame() {
  const frame = native.getContext();
  return typeof frame === 'object' && frame !== null ? frame : null;
}

function frameWith(key, value) {
  const next = { __proto__: null };
  const frame = currentFrame();
  if (frame !== null) {
    for (const existing of Object.getOwnPropertySymbols(frame)) {
      next[existing] = frame[existing];
    }
  }
  next[key] = value;
  return next;
}

class AsyncLocalStorage {
  #key = Symbol('AsyncLocalStorage');
  #enabled = true;

  run(store, callback, ...args) {
    if (typeof callback !== 'function') {
      const err = new TypeError('The "callback" argument must be of type function.');
      err.code = 'ERR_INVALID_ARG_TYPE';
      throw err;
    }
    const previous = native.getContext();
    native.setContext(frameWith(this.#key, { [kStore]: store }));
    try {
      return callback(...args);
    } finally {
      native.setContext(previous);
    }
  }

  // `exit` is `run` with the binding removed rather than replaced.
  exit(callback, ...args) {
    const previous = native.getContext();
    native.setContext(frameWith(this.#key, undefined));
    try {
      return callback(...args);
    } finally {
      native.setContext(previous);
    }
  }

  // Unlike `run`, this has no scope: the binding stays until the current
  // synchronous execution unwinds into a context that never saw it.
  enterWith(store) {
    native.setContext(frameWith(this.#key, { [kStore]: store }));
  }

  getStore() {
    if (!this.#enabled) return undefined;
    const frame = currentFrame();
    if (frame === null) return undefined;
    const bound = frame[this.#key];
    return bound === undefined ? undefined : bound[kStore];
  }

  disable() {
    this.#enabled = false;
  }

  // The returned function runs `callback` in the context captured now.
  bind(callback) {
    const captured = native.getContext();
    return function bound(...args) {
      const previous = native.getContext();
      native.setContext(captured);
      try {
        return callback.apply(this, args);
      } finally {
        native.setContext(previous);
      }
    };
  }

  static bind(fn) {
    const captured = native.getContext();
    return function bound(...args) {
      const previous = native.getContext();
      native.setContext(captured);
      try {
        return fn.apply(this, args);
      } finally {
        native.setContext(previous);
      }
    };
  }

  static snapshot() {
    const captured = native.getContext();
    return function runInSnapshot(fn, ...args) {
      const previous = native.getContext();
      native.setContext(captured);
      try {
        return fn(...args);
      } finally {
        native.setContext(previous);
      }
    };
  }
}

module.exports = {
  AsyncLocalStorage,
  AsyncResource,
  createHook,
  executionAsyncId,
  triggerAsyncId,
  executionAsyncResource,
};
