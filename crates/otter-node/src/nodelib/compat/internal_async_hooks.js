'use strict';
// internal/async_hooks — the async id machinery every other module hangs
// off: one monotonic id space, an execution stack that answers
// `executionAsyncId`/`triggerAsyncId`, and the hook list `createHook`
// installs. `node:async_hooks` is the public face of this module, and the
// resource wrappers (timers, stream handles) emit through it.
//
// A resource's `destroy` fires either from an explicit `emitDestroy` or,
// for a resource nobody kept, from the finalization registry once the
// collector has taken it — which is how a caller watches an object become
// unreachable.

const async_id_symbol = Symbol('asyncId');
const trigger_async_id_symbol = Symbol('triggerAsyncId');
const owner_symbol = Symbol('owner');
const init_symbol = Symbol('init');
const before_symbol = Symbol('before');
const after_symbol = Symbol('after');
const destroy_symbol = Symbol('destroy');
const promise_resolve_symbol = Symbol('promiseResolve');
const async_context_symbol = Symbol('asyncContext');

// The isolate's async context travels with every scheduled callback; a
// resource re-enters the one it was created in, which is what makes
// `AsyncLocalStorage` survive a `runInAsyncScope`.
const contextNative = globalThis.__otterAsyncContextNative;

const kRootAsyncId = 1;
let idCounter = kRootAsyncId;

// Innermost entry is the callback currently running. The root entry is the
// program itself, which every top-level operation triggers.
const executionStack = [{ asyncId: kRootAsyncId, triggerAsyncId: 0, resource: null }];
const activeHooks = [];
const destroyed = new Set();

// Resources whose `destroy` is owed to the collector: the registry holds the
// id, never the resource, so registration cannot keep it alive.
const finalizers = new FinalizationRegistry((asyncId) => {
  emitDestroy(asyncId);
});

function newAsyncId() {
  return ++idCounter;
}

function executionAsyncId() {
  return executionStack[executionStack.length - 1].asyncId;
}

function triggerAsyncId() {
  return executionStack[executionStack.length - 1].triggerAsyncId;
}

function executionAsyncResource() {
  return executionStack[executionStack.length - 1].resource;
}

function enabledHooksExist() {
  return activeHooks.length > 0;
}

function callHook(hook, symbol, args) {
  const callback = hook[symbol];
  if (typeof callback !== 'function') return;
  // Hooks run as methods of their `AsyncHook`, which is where a caller
  // parks state between `init` and `destroy`.
  Reflect.apply(callback, hook, args);
}

function emitInit(asyncId, type, triggerId, resource) {
  if (activeHooks.length === 0) return;
  for (const hook of activeHooks.slice()) {
    callHook(hook, init_symbol, [asyncId, type, triggerId, resource]);
  }
}

function emitBefore(asyncId) {
  if (activeHooks.length === 0) return;
  for (const hook of activeHooks.slice()) {
    callHook(hook, before_symbol, [asyncId]);
  }
}

function emitAfter(asyncId) {
  if (activeHooks.length === 0) return;
  for (const hook of activeHooks.slice()) {
    callHook(hook, after_symbol, [asyncId]);
  }
}

function emitDestroy(asyncId) {
  if (typeof asyncId !== 'number' || destroyed.has(asyncId)) return;
  destroyed.add(asyncId);
  if (activeHooks.length === 0) return;
  for (const hook of activeHooks.slice()) {
    callHook(hook, destroy_symbol, [asyncId]);
  }
}

function emitPromiseResolve(asyncId) {
  if (activeHooks.length === 0) return;
  for (const hook of activeHooks.slice()) {
    callHook(hook, promise_resolve_symbol, [asyncId]);
  }
}

/// Run `block` as the callback identified by `asyncId`, with `before` and
/// `after` around it, so nested operations are triggered by this one.
function runInAsyncScope(asyncId, triggerId, resource, block, thisArg, args) {
  executionStack.push({ asyncId, triggerAsyncId: triggerId, resource });
  const previousContext = contextNative?.getContext();
  const captured = resource?.[async_context_symbol];
  if (captured !== undefined) contextNative?.setContext(captured);
  emitBefore(asyncId);
  try {
    return Reflect.apply(block, thisArg, args);
  } finally {
    emitAfter(asyncId);
    if (captured !== undefined) contextNative?.setContext(previousContext);
    executionStack.pop();
  }
}

/// Report an operation whose callbacks are owned elsewhere; the returned id
/// is what the owner later passes to `runInAsyncScope`/`emitDestroy`.
function registerAsyncResource(resource, type, triggerId = executionAsyncId()) {
  const asyncId = newAsyncId();
  resource[async_id_symbol] = asyncId;
  resource[trigger_async_id_symbol] = triggerId;
  emitInit(asyncId, type, triggerId, resource);
  finalizers.register(resource, asyncId, resource);
  return asyncId;
}

function unregisterAsyncResource(resource) {
  finalizers.unregister(resource);
}

class AsyncHook {
  constructor(callbacks = {}) {
    const { init, before, after, destroy, promiseResolve } = callbacks;
    for (const [name, fn] of [
      ['init', init], ['before', before], ['after', after],
      ['destroy', destroy], ['promiseResolve', promiseResolve],
    ]) {
      if (fn !== undefined && typeof fn !== 'function') {
        const error = new TypeError(`The "options.${name}" property must be of type function.`);
        error.code = 'ERR_ASYNC_CALLBACK';
        throw error;
      }
    }
    this[init_symbol] = init;
    this[before_symbol] = before;
    this[after_symbol] = after;
    this[destroy_symbol] = destroy;
    this[promise_resolve_symbol] = promiseResolve;
  }

  enable() {
    if (!activeHooks.includes(this)) activeHooks.push(this);
    return this;
  }

  disable() {
    const at = activeHooks.indexOf(this);
    if (at !== -1) activeHooks.splice(at, 1);
    return this;
  }
}

function createHook(callbacks) {
  return new AsyncHook(callbacks);
}

class AsyncResource {
  constructor(type, options = {}) {
    if (typeof type !== 'string') {
      const error = new TypeError('The "type" argument must be of type string.');
      error.code = 'ERR_INVALID_ARG_TYPE';
      throw error;
    }
    const settings = typeof options === 'number'
      ? { triggerAsyncId: options }
      : (options ?? {});
    const triggerId = typeof settings.triggerAsyncId === 'number'
      ? settings.triggerAsyncId
      : executionAsyncId();
    this[owner_symbol] = this;
    this[async_context_symbol] = contextNative?.getContext();
    this._type = type;
    this._requireManualDestroy = settings.requireManualDestroy === true;
    registerAsyncResource(this, type, triggerId);
  }

  runInAsyncScope(fn, thisArg, ...args) {
    return runInAsyncScope(
      this[async_id_symbol],
      this[trigger_async_id_symbol],
      this,
      fn,
      thisArg,
      args,
    );
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

  emitDestroy() {
    unregisterAsyncResource(this);
    emitDestroy(this[async_id_symbol]);
    return this;
  }

  asyncId() { return this[async_id_symbol]; }
  triggerAsyncId() { return this[trigger_async_id_symbol]; }
  get type() { return this._type; }

  static bind(fn, type, thisArg) {
    const resource = new AsyncResource(type ?? fn.name ?? 'bound-anonymous-fn');
    return resource.bind(fn, thisArg);
  }
}

module.exports = {
  enabledHooksExist,
  // What a resource created right now would name as the reason it exists:
  // the trigger the current scope was given, or the scope itself.
  getDefaultTriggerAsyncId() {
    const current = executionStack[executionStack.length - 1];
    return current.triggerAsyncId || current.asyncId;
  },
  // Whether anything is listening for a resource being created. Nothing is
  // emitted when nobody is.
  initHooksExist: enabledHooksExist,
  defaultTriggerAsyncIdScope(triggerAsyncId, block, ...args) {
    const current = executionStack[executionStack.length - 1];
    executionStack.push({
      asyncId: current.asyncId,
      triggerAsyncId,
      resource: current.resource,
    });
    try {
      return block(...args);
    } finally {
      executionStack.pop();
    }
  },
  getOrSetAsyncId(object) {
    if (typeof object[async_id_symbol] !== 'number') {
      object[async_id_symbol] = newAsyncId();
    }
    return object[async_id_symbol];
  },
  newAsyncId,
  executionAsyncId,
  triggerAsyncId,
  executionAsyncResource,
  emitInit,
  emitBefore,
  emitAfter,
  emitDestroy,
  emitPromiseResolve,
  registerAsyncResource,
  unregisterAsyncResource,
  runInAsyncScope,
  createHook,
  AsyncHook,
  AsyncResource,
  symbols: {
    async_id_symbol,
    trigger_async_id_symbol,
    owner_symbol,
    async_context_symbol,
  },
};
