'use strict';
// Node-shaped timer globals: setTimeout/setInterval hand back a Timeout
// object carrying ref/unref/refresh, wrapping the runtime's numeric tokens.
// ref/unref move the token between the host scheduler's liveness classes via
// the __otterTimerSetRef native, so an unref'd timer stops holding the
// run-until-idle boundary open.
(() => {
  const origSetTimeout = globalThis.setTimeout;
  const origClearTimeout = globalThis.clearTimeout;
  const origSetInterval = globalThis.setInterval;
  const origClearInterval = globalThis.clearInterval;

  class Timeout {
    constructor(repeat, callback, delay, args) {
      this._repeat = repeat;
      this._callback = callback;
      this._delay = delay;
      this._args = args;
      this._destroyed = false;
      this._refed = true;
      this._id = repeat
        ? origSetInterval(callback, delay, ...args)
        : origSetTimeout(callback, delay, ...args);
    }
    ref() {
      this._refed = true;
      if (!this._destroyed) __otterTimerSetRef(this._id, true);
      return this;
    }
    unref() {
      this._refed = false;
      if (!this._destroyed) __otterTimerSetRef(this._id, false);
      return this;
    }
    hasRef() { return this._refed; }
    refresh() {
      if (this._destroyed) return this;
      (this._repeat ? origClearInterval : origClearTimeout)(this._id);
      this._id = this._repeat
        ? origSetInterval(this._callback, this._delay, ...this._args)
        : origSetTimeout(this._callback, this._delay, ...this._args);
      if (!this._refed) __otterTimerSetRef(this._id, false);
      return this;
    }
    close() { clearWrapped(this, this._repeat); return this; }
    [Symbol.toPrimitive]() { return this._id; }
  }

  function clearWrapped(value, repeat) {
    if (value == null) return;
    if (typeof value === 'object') {
      value._destroyed = true;
      (repeat ? origClearInterval : origClearTimeout)(value._id);
      return;
    }
    (repeat ? origClearInterval : origClearTimeout)(value);
  }

  globalThis.setTimeout = function setTimeout(callback, delay, ...args) {
    return new Timeout(false, callback, delay, args);
  };
  globalThis.clearTimeout = function clearTimeout(value) {
    clearWrapped(value, false);
  };
  globalThis.setInterval = function setInterval(callback, delay, ...args) {
    return new Timeout(true, callback, delay, args);
  };
  globalThis.clearInterval = function clearInterval(value) {
    clearWrapped(value, true);
  };
})();
