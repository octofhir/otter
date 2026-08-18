'use strict';
// `node:domain` — group asynchronous work so its errors reach one handler.
//
// The active domain rides the engine's async context, so a callback scheduled
// inside `d.run()` still reports to `d` when it finally runs. The runtime asks
// this module where to send an uncaught error through `_errorHandler`.

const EventEmitter = require('events');
const asyncHooks = require('async_hooks');

const storage = new asyncHooks.AsyncLocalStorage();
const stack = [];

class Domain extends EventEmitter {
  constructor() {
    super();
    this.members = [];
    this._disposed = false;
  }

  // Enter without a scope: the domain stays active until `exit`, which is what
  // lets a caller straddle several statements.
  enter() {
    if (this._disposed) return;
    exports.active = this;
    process.domain = this;
    stack.push(this);
    storage.enterWith(this);
  }

  exit() {
    const index = stack.lastIndexOf(this);
    if (index === -1) return;
    stack.splice(index, stack.length - index);
    const next = stack[stack.length - 1];
    exports.active = next;
    process.domain = next;
    storage.enterWith(next);
  }

  run(fn, ...args) {
    if (this._disposed) return undefined;
    stack.push(this);
    const previousActive = exports.active;
    const previousProcessDomain = process.domain;
    exports.active = this;
    process.domain = this;
    try {
      return storage.run(this, () => fn.apply(this, args));
    } catch (error) {
      this._errorHandler(error);
      return undefined;
    } finally {
      const index = stack.lastIndexOf(this);
      if (index !== -1) stack.splice(index, 1);
      exports.active = previousActive;
      process.domain = previousProcessDomain;
    }
  }

  // An emitter added to a domain reports its `error` events here instead of
  // throwing when nobody else is listening.
  add(emitter) {
    if (emitter === null || typeof emitter !== 'object') return;
    if (emitter.domain === this) return;
    if (emitter.domain) emitter.domain.remove(emitter);
    emitter.domain = this;
    this.members.push(emitter);
    if (typeof emitter.on === 'function') {
      emitter.on('error', (error) => this._errorHandler(error));
    }
  }

  remove(emitter) {
    if (emitter === null || typeof emitter !== 'object') return;
    if (emitter.domain === this) emitter.domain = null;
    const index = this.members.indexOf(emitter);
    if (index !== -1) this.members.splice(index, 1);
  }

  bind(callback) {
    const domain = this;
    return function boundInDomain(...args) {
      return domain.run(() => callback.apply(this, args));
    };
  }

  // Node's `intercept` also swallows the error-first argument: a truthy first
  // argument goes to the domain and the callback never runs.
  intercept(callback) {
    const domain = this;
    return function interceptedInDomain(error, ...args) {
      if (error) {
        domain._errorHandler(error);
        return undefined;
      }
      return domain.run(() => callback.apply(this, args));
    };
  }

  dispose() {
    this._disposed = true;
    for (const member of this.members.slice()) this.remove(member);
    this.exit();
  }

  // The single place an error enters a domain, whether it came from a member
  // emitter, a bound callback, or the runtime's uncaught path.
  _errorHandler(error) {
    if (error !== null && typeof error === 'object') {
      try {
        error.domain = this;
        error.domainThrown = true;
      } catch {
        // A frozen error keeps its own shape; the emit below still happens.
      }
    }
    if (this.listenerCount('error') === 0) return false;
    this.emit('error', error);
    return true;
  }
}

function create() {
  return new Domain();
}

// The runtime calls this before treating an error as fatal. Answering `true`
// means a domain took ownership.
function _handleUncaught(error) {
  const active = storage.getStore() ?? exports.active ?? stack[stack.length - 1];
  if (!active) return false;
  return active._errorHandler(error);
}

exports.Domain = Domain;
exports.create = create;
exports.createDomain = create;
exports.active = null;
exports._stack = stack;
exports._handleUncaught = _handleUncaught;

// `domain.run` without a receiver runs on a fresh domain, matching the module's
// documented shape.
exports.run = function run(fn, ...args) {
  return create().run(fn, ...args);
};

globalThis.__otterDomainModule = exports;
Object.defineProperty(globalThis, '__otterDomainModule', { enumerable: false });
