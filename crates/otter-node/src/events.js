'use strict';
// `node:events` — a faithful, self-contained EventEmitter implementation.
// Pure JS (no native deps), executed through `run_builtin_cjs_shim`.

let defaultMaxListeners = 10;

const kRejection = Symbol.for('nodejs.rejection');
const errorMonitor = Symbol('events.errorMonitor');
const captureRejectionSymbol = Symbol.for('nodejs.rejection');

function EventEmitter(opts) {
  EventEmitter.init.call(this, opts);
}

EventEmitter.EventEmitter = EventEmitter;
EventEmitter.errorMonitor = errorMonitor;
EventEmitter.captureRejectionSymbol = captureRejectionSymbol;

Object.defineProperty(EventEmitter, 'defaultMaxListeners', {
  enumerable: true,
  get() { return defaultMaxListeners; },
  set(arg) {
    if (typeof arg !== 'number' || arg < 0 || Number.isNaN(arg)) {
      const err = new RangeError(
        `The value of "defaultMaxListeners" is out of range. It must be a non-negative number. Received ${arg}.`);
      err.code = 'ERR_OUT_OF_RANGE';
      throw err;
    }
    defaultMaxListeners = arg;
  },
});

EventEmitter.init = function init(opts) {
  if (this._events === undefined ||
      this._events === Object.getPrototypeOf(this)._events) {
    this._events = { __proto__: null };
    this._eventsCount = 0;
  }
  this._maxListeners = this._maxListeners || undefined;
  if (opts && opts.captureRejections) {
    this[kCapture] = Boolean(opts.captureRejections);
  }
};

const kCapture = Symbol('kCapture');

function checkListener(listener) {
  if (typeof listener !== 'function') {
    const err = new TypeError(
      `The "listener" argument must be of type function. Received ${typeof listener}`);
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
}

EventEmitter.prototype._events = undefined;
EventEmitter.prototype._eventsCount = 0;
EventEmitter.prototype._maxListeners = undefined;

EventEmitter.prototype.setMaxListeners = function setMaxListeners(n) {
  if (typeof n !== 'number' || n < 0 || Number.isNaN(n)) {
    const err = new RangeError(
      `The value of "n" is out of range. It must be a non-negative number. Received ${n}.`);
    err.code = 'ERR_OUT_OF_RANGE';
    throw err;
  }
  this._maxListeners = n;
  return this;
};

EventEmitter.prototype.getMaxListeners = function getMaxListeners() {
  return this._maxListeners === undefined ? defaultMaxListeners : this._maxListeners;
};

function _addListener(target, type, listener, prepend) {
  checkListener(listener);
  let events = target._events;
  if (events === undefined) {
    events = target._events = { __proto__: null };
    target._eventsCount = 0;
  } else if (events.newListener !== undefined) {
    target.emit('newListener', type,
                listener.listener ? listener.listener : listener);
    events = target._events;
  }

  let existing = events[type];
  if (existing === undefined) {
    events[type] = listener;
    ++target._eventsCount;
  } else {
    if (typeof existing === 'function') {
      existing = events[type] = prepend ? [listener, existing] : [existing, listener];
    } else if (prepend) {
      existing.unshift(listener);
    } else {
      existing.push(listener);
    }
  }
  return target;
}

EventEmitter.prototype.addListener = function addListener(type, listener) {
  return _addListener(this, type, listener, false);
};
EventEmitter.prototype.on = EventEmitter.prototype.addListener;

EventEmitter.prototype.prependListener = function prependListener(type, listener) {
  return _addListener(this, type, listener, true);
};

function onceWrapper() {
  if (!this.fired) {
    this.target.removeListener(this.type, this.wrapFn);
    this.fired = true;
    if (arguments.length === 0) return this.listener.call(this.target);
    return this.listener.apply(this.target, arguments);
  }
}

function _onceWrap(target, type, listener) {
  const state = { fired: false, wrapFn: undefined, target, type, listener };
  const wrapped = onceWrapper.bind(state);
  wrapped.listener = listener;
  state.wrapFn = wrapped;
  return wrapped;
}

EventEmitter.prototype.once = function once(type, listener) {
  checkListener(listener);
  this.on(type, _onceWrap(this, type, listener));
  return this;
};

EventEmitter.prototype.prependOnceListener = function prependOnceListener(type, listener) {
  checkListener(listener);
  this.prependListener(type, _onceWrap(this, type, listener));
  return this;
};

EventEmitter.prototype.removeListener = function removeListener(type, listener) {
  checkListener(listener);
  const events = this._events;
  if (events === undefined) return this;
  const list = events[type];
  if (list === undefined) return this;

  if (list === listener || list.listener === listener) {
    if (--this._eventsCount === 0) {
      this._events = { __proto__: null };
    } else {
      delete events[type];
      if (events.removeListener)
        this.emit('removeListener', type, list.listener || listener);
    }
  } else if (typeof list !== 'function') {
    let position = -1;
    for (let i = list.length - 1; i >= 0; i--) {
      if (list[i] === listener || list[i].listener === listener) {
        position = i;
        break;
      }
    }
    if (position < 0) return this;
    if (position === 0) list.shift();
    else list.splice(position, 1);
    if (list.length === 1) events[type] = list[0];
    if (events.removeListener !== undefined)
      this.emit('removeListener', type, listener);
  }
  return this;
};
EventEmitter.prototype.off = EventEmitter.prototype.removeListener;

EventEmitter.prototype.removeAllListeners = function removeAllListeners(type) {
  const events = this._events;
  if (events === undefined) return this;
  if (events.removeListener === undefined) {
    if (arguments.length === 0) {
      this._events = { __proto__: null };
      this._eventsCount = 0;
    } else if (events[type] !== undefined) {
      if (--this._eventsCount === 0) this._events = { __proto__: null };
      else delete events[type];
    }
    return this;
  }
  // Emit removeListener for all, last to first.
  if (arguments.length === 0) {
    for (const key of Object.keys(events)) {
      if (key === 'removeListener') continue;
      this.removeAllListeners(key);
    }
    this.removeAllListeners('removeListener');
    this._events = { __proto__: null };
    this._eventsCount = 0;
    return this;
  }
  const listeners = events[type];
  if (typeof listeners === 'function') {
    this.removeListener(type, listeners);
  } else if (listeners !== undefined) {
    for (let i = listeners.length - 1; i >= 0; i--) {
      this.removeListener(type, listeners[i]);
    }
  }
  return this;
};

function arrayClone(arr) {
  const copy = new Array(arr.length);
  for (let i = 0; i < arr.length; i++) copy[i] = arr[i];
  return copy;
}

EventEmitter.prototype.emit = function emit(type, ...args) {
  const events = this._events;
  const doError = (type === 'error');
  if (events !== undefined) {
    if (doError && events[errorMonitor] !== undefined)
      this.emit(errorMonitor, ...args);
  } else if (!doError) {
    return false;
  }

  if (doError) {
    const handler = events && events.error;
    if (handler === undefined) {
      const er = args.length > 0 ? args[0] : undefined;
      if (er instanceof Error) throw er;
      const err = new Error(`Unhandled error.${er ? ` (${er.message || er})` : ''}`);
      err.context = er;
      throw err;
    }
  }

  const handler = events[type];
  if (handler === undefined) return false;

  if (typeof handler === 'function') {
    const result = handler.apply(this, args);
    if (result !== undefined && result !== null) void result;
  } else {
    const listeners = arrayClone(handler);
    for (let i = 0; i < listeners.length; i++) {
      listeners[i].apply(this, args);
    }
  }
  return true;
};

function _listeners(target, type, unwrap) {
  const events = target._events;
  if (events === undefined) return [];
  const evlistener = events[type];
  if (evlistener === undefined) return [];
  if (typeof evlistener === 'function')
    return unwrap ? [evlistener.listener || evlistener] : [evlistener];
  return unwrap ? unwrapListeners(evlistener) : arrayClone(evlistener);
}

function unwrapListeners(arr) {
  const ret = new Array(arr.length);
  for (let i = 0; i < arr.length; i++) ret[i] = arr[i].listener || arr[i];
  return ret;
}

EventEmitter.prototype.listeners = function listeners(type) {
  return _listeners(this, type, true);
};
EventEmitter.prototype.rawListeners = function rawListeners(type) {
  return _listeners(this, type, false);
};

EventEmitter.prototype.listenerCount = function listenerCount(type, listener) {
  const events = this._events;
  if (events === undefined) return 0;
  const evlistener = events[type];
  if (evlistener === undefined) return 0;
  if (typeof evlistener === 'function') {
    if (listener !== undefined)
      return (listener === evlistener || listener === evlistener.listener) ? 1 : 0;
    return 1;
  }
  if (listener !== undefined) {
    let matching = 0;
    for (let i = 0; i < evlistener.length; i++) {
      if (evlistener[i] === listener || evlistener[i].listener === listener) matching++;
    }
    return matching;
  }
  return evlistener.length;
};

EventEmitter.prototype.eventNames = function eventNames() {
  return this._eventsCount > 0 ? Reflect.ownKeys(this._events) : [];
};

// ---- statics ----

EventEmitter.listenerCount = function listenerCount(emitter, type) {
  return emitter.listenerCount(type);
};

EventEmitter.getMaxListeners = function getMaxListeners(emitterOrTarget) {
  if (emitterOrTarget && typeof emitterOrTarget.getMaxListeners === 'function')
    return emitterOrTarget.getMaxListeners();
  if (emitterOrTarget && typeof emitterOrTarget[kMaxEventTargetListeners] === 'number')
    return emitterOrTarget[kMaxEventTargetListeners];
  return defaultMaxListeners;
};

const kMaxEventTargetListeners = Symbol('events.maxEventTargetListeners');

EventEmitter.setMaxListeners = function setMaxListeners(n = defaultMaxListeners, ...eventTargets) {
  if (typeof n !== 'number' || n < 0 || Number.isNaN(n)) {
    const err = new RangeError(
      `The value of "n" is out of range. It must be a non-negative number. Received ${n}.`);
    err.code = 'ERR_OUT_OF_RANGE';
    throw err;
  }
  if (eventTargets.length === 0) {
    defaultMaxListeners = n;
    return;
  }
  for (const target of eventTargets) {
    if (target && typeof target.setMaxListeners === 'function') {
      target.setMaxListeners(n);
    } else if (target) {
      target[kMaxEventTargetListeners] = n;
    }
  }
};

EventEmitter.once = function once(emitter, name, options) {
  return new Promise((resolve, reject) => {
    const signal = options ? options.signal : undefined;
    if (signal && signal.aborted) {
      return reject(abortError(signal));
    }
    const errorListener = (err) => {
      emitter.removeListener(name, resolver);
      reject(err);
    };
    const resolver = (...args) => {
      if (typeof emitter.removeListener === 'function')
        emitter.removeListener('error', errorListener);
      resolve(args);
    };
    if (typeof emitter.once === 'function') {
      emitter.once(name, resolver);
      if (name !== 'error') emitter.once('error', errorListener);
    } else if (typeof emitter.addEventListener === 'function') {
      emitter.addEventListener(name, (ev) => resolve([ev]), { once: true });
    }
  });
};

function abortError(signal) {
  const reason = signal.reason;
  const err = reason !== undefined ? reason : new Error('The operation was aborted');
  if (!reason) err.name = 'AbortError';
  return err;
}

EventEmitter.on = function on(emitter, name) {
  const unconsumed = [];
  const queued = [];
  let error = null;
  let finished = false;

  const iterator = {
    next() {
      if (queued.length) {
        return Promise.resolve({ value: queued.shift(), done: false });
      }
      if (error) {
        const p = Promise.reject(error);
        error = null;
        return p;
      }
      if (finished) return Promise.resolve({ value: undefined, done: true });
      return new Promise((resolve, reject) => unconsumed.push({ resolve, reject }));
    },
    return() {
      finished = true;
      emitter.removeListener(name, eventHandler);
      return Promise.resolve({ value: undefined, done: true });
    },
    throw(err) { error = err; },
    [Symbol.asyncIterator]() { return this; },
  };

  function eventHandler(...args) {
    const waiting = unconsumed.shift();
    if (waiting) waiting.resolve({ value: args, done: false });
    else queued.push(args);
  }
  emitter.on(name, eventHandler);
  return iterator;
};

EventEmitter.getEventListeners = function getEventListeners(emitterOrTarget, name) {
  if (emitterOrTarget && typeof emitterOrTarget.listeners === 'function')
    return emitterOrTarget.listeners(name);
  return [];
};

module.exports = EventEmitter;
module.exports.EventEmitter = EventEmitter;
module.exports.defaultMaxListeners = defaultMaxListeners;
module.exports.once = EventEmitter.once;
module.exports.on = EventEmitter.on;
module.exports.getEventListeners = EventEmitter.getEventListeners;
module.exports.getMaxListeners = EventEmitter.getMaxListeners;
module.exports.setMaxListeners = EventEmitter.setMaxListeners;
module.exports.errorMonitor = errorMonitor;
module.exports.captureRejectionSymbol = captureRejectionSymbol;
