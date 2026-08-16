'use strict';
// `node:async_hooks` — asynchronous context tracking.
//
// The engine carries one context value per isolate: a microtask or timer
// captures it when it is queued and restores it around its own execution. A
// store is therefore a plain frame object, and entering one means installing a
// copy with an extra binding — the copy is what later continuations inherit.

const native = globalThis.__otterAsyncContextNative;

const kStore = Symbol('kStore');
let nextResourceId = 1;

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

class AsyncResource {
  #context;
  #id;
  #type;

  constructor(type, options = {}) {
    if (typeof type !== 'string') {
      const err = new TypeError('The "type" argument must be of type string.');
      err.code = 'ERR_INVALID_ARG_TYPE';
      throw err;
    }
    this.#type = type;
    this.#id = nextResourceId++;
    // The resource remembers the context it was created in; every
    // `runInAsyncScope` re-enters that one.
    this.#context = native.getContext();
    void options;
  }

  runInAsyncScope(fn, thisArg, ...args) {
    const previous = native.getContext();
    native.setContext(this.#context);
    try {
      return fn.apply(thisArg, args);
    } finally {
      native.setContext(previous);
    }
  }

  bind(fn, thisArg) {
    const resource = this;
    const bound = function boundAsyncResource(...args) {
      return resource.runInAsyncScope(fn, thisArg ?? this, ...args);
    };
    Object.defineProperty(bound, 'length', { value: fn.length, configurable: true });
    bound.asyncResource = this;
    return bound;
  }

  asyncId() { return this.#id; }
  triggerAsyncId() { return 0; }
  emitDestroy() { return this; }
  get type() { return this.#type; }

  static bind(fn, type, thisArg) {
    const resource = new AsyncResource(type ?? fn.name ?? 'bound-anonymous-fn');
    return resource.bind(fn, thisArg);
  }
}

// `executionAsyncId` needs per-callback ids the engine does not assign yet, so
// it reports the root. `createHook` is deliberately absent rather than a stub
// that never fires: a caller can then detect it instead of silently observing
// nothing.
function executionAsyncId() { return 1; }
function triggerAsyncId() { return 0; }
function executionAsyncResource() { return currentFrame() ?? Object.create(null); }

module.exports = {
  AsyncLocalStorage,
  AsyncResource,
  executionAsyncId,
  triggerAsyncId,
  executionAsyncResource,
};
