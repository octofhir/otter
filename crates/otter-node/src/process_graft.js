'use strict';
const { EventEmitter } = require('events');
const target = globalThis.process;
if (typeof target === 'object' && !(target instanceof EventEmitter)) {
  const ProcessCtor = function process() {};
  Object.setPrototypeOf(ProcessCtor.prototype, EventEmitter.prototype);
  Object.setPrototypeOf(ProcessCtor, EventEmitter);
  Object.setPrototypeOf(target, ProcessCtor.prototype);
}
if (typeof target === 'object' && typeof target.getBuiltinModule !== 'function') {
  const Module = require('module');
  const realmRequire = require;
  Object.defineProperty(target, 'getBuiltinModule', {
    value: function getBuiltinModule(id) {
      if (typeof id !== 'string') {
        const suffix = id === null ? ' Received null'
          : ` Received type ${typeof id}`;
        const err = new TypeError(
          `The "id" argument must be of type string.${suffix}`);
        err.code = 'ERR_INVALID_ARG_TYPE';
        throw err;
      }
      if (!Module.isBuiltin(id)) return undefined;
      return realmRequire(id);
    },
    writable: true,
    enumerable: false,
    configurable: true,
  });
}
if (typeof target === 'object') {
  const { internalBinding } = require('internal/bootstrap/realm');
  const utilTypes = require('util').types;
  const utilBindingKeys = [
    'isAnyArrayBuffer', 'isArrayBuffer', 'isArrayBufferView', 'isAsyncFunction',
    'isDataView', 'isDate', 'isExternal', 'isMap', 'isMapIterator',
    'isNativeError', 'isPromise', 'isRegExp', 'isSet', 'isSetIterator',
    'isTypedArray', 'isUint8Array',
  ];
  const allowedBindings = [
    'buffer', 'cares_wrap', 'constants', 'contextify', 'fs', 'fs_event_wrap',
    'icu', 'inspector', 'js_stream', 'natives', 'os', 'pipe_wrap',
    'spawn_sync', 'stream_wrap', 'tcp_wrap', 'tls_wrap', 'tty_wrap',
    'udp_wrap', 'util', 'uv', 'zlib',
  ];
  const cache = new Map();
  Object.defineProperty(target, 'binding', {
    value: function binding(name) {
      name = String(name);
      if (cache.has(name)) return cache.get(name);
      let bound;
      if (name === 'util') {
        bound = {};
        for (const key of utilBindingKeys) bound[key] = utilTypes[key];
      } else if (allowedBindings.includes(name)) {
        try {
          bound = internalBinding(name);
        } catch {
          bound = undefined;
        }
        if (bound === undefined || bound === null) bound = {};
      } else {
        throw new Error(`No such module: ${name}`);
      }
      cache.set(name, bound);
      return bound;
    },
    writable: true,
    enumerable: false,
    configurable: true,
  });
}
