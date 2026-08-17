'use strict';
// internal/async_hooks — no hooks exist in this realm, so id scoping is a
// plain call and ids are a monotonic counter.
const async_id_symbol = Symbol('asyncId');
const trigger_async_id_symbol = Symbol('triggerAsyncId');
const owner_symbol = Symbol('owner');

let idCounter = 1;

module.exports = {
  enabledHooksExist() { return false; },
  defaultTriggerAsyncIdScope(_triggerAsyncId, block, ...args) {
    return block(...args);
  },
  getOrSetAsyncId(object) {
    if (typeof object[async_id_symbol] !== 'number') {
      object[async_id_symbol] = ++idCounter;
    }
    return object[async_id_symbol];
  },
  newAsyncId() { return ++idCounter; },
  symbols: { async_id_symbol, trigger_async_id_symbol, owner_symbol },
  AsyncResource: class AsyncResource {
    constructor() {}
    runInAsyncScope(fn, thisArg, ...args) { return Reflect.apply(fn, thisArg, args); }
    emitDestroy() { return this; }
  },
};
