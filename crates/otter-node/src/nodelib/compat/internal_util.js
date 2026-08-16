'use strict';
// The slice of `internal/util` the vendored files import.
const util = require('util');

const customInspectSymbol = Symbol.for('nodejs.util.inspect.custom');
const customPromisifyArgs = Symbol('customPromisifyArgs');
const kEmptyObject = Object.freeze(Object.create(null));
const kEnumerableProperty = Object.create(null);
kEnumerableProperty.enumerable = true;
Object.freeze(kEnumerableProperty);

function once(callback, { preserveReturnValue = false } = {}) {
  let called = false;
  let returnValue;
  return function (...args) {
    if (called) return returnValue;
    called = true;
    const result = Reflect.apply(callback, this, args);
    if (preserveReturnValue) returnValue = result;
    return result;
  };
}

function deprecate(fn, message, code) {
  let warned = false;
  function deprecated(...args) {
    if (!warned) {
      warned = true;
      process.emitWarning(message, 'DeprecationWarning', code);
    }
    if (new.target) return Reflect.construct(fn, args, new.target);
    return Reflect.apply(fn, this, args);
  }
  return deprecated;
}

function createDeferredPromise() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

const promisify = util.promisify ?? ((fn) => fn);
if (promisify.custom === undefined) {
  promisify.custom = Symbol.for('nodejs.util.promisify.custom');
}

function assignFunctionName(name, fn) {
  const value = typeof name === 'symbol'
    ? `[${name.description ?? 'Symbol()'}]`
    : String(name);
  try {
    Object.defineProperty(fn, 'name', { value, configurable: true });
  } catch {
    // A frozen function keeps its own name.
  }
  return fn;
}

module.exports = {
  assignFunctionName,
  customInspectSymbol,
  customPromisifyArgs,
  kEmptyObject,
  kEnumerableProperty,
  once,
  deprecate,
  createDeferredPromise,
  promisify,
  normalizeEncoding(enc) {
    if (enc == null || enc === 'utf8' || enc === 'utf-8') return 'utf8';
    return String(enc).toLowerCase();
  },
  getInternalGlobal(name) { return globalThis[name]; },
  SideEffectFreeRegExpPrototypeExec: (re, str) => RegExp.prototype.exec.call(re, str),
  SideEffectFreeRegExpPrototypeSymbolReplace: (re, str, replacement) =>
    RegExp.prototype[Symbol.replace].call(re, str, replacement),
  isError(value) { return value instanceof Error; },
  emitExperimentalWarning() {},
  assertCrypto() {},
  join(output, separator) { return output.join(separator); },
  defineOperation(target, name, method) { target[name] = method; },
  defineLazyProperties() {},
  exposeInterface() {},
  lazyDOMException(message, name) {
    const err = new Error(message);
    err.name = name;
    return err;
  },
};
