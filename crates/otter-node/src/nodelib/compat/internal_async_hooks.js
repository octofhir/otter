'use strict';
module.exports = {
  enabledHooksExist() { return false; },
  AsyncResource: class AsyncResource {
    constructor() {}
    runInAsyncScope(fn, thisArg, ...args) { return Reflect.apply(fn, thisArg, args); }
    emitDestroy() { return this; }
  },
};
