'use strict';
// The slice of `internal/util` the vendored files import. Self-contained on
// purpose: the public `util` module is itself vendored and requires this
// file, so anything here that required `util` back would meet a
// half-initialized exports object in the cycle.

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

const kCustomPromisifiedSymbol = Symbol.for('nodejs.util.promisify.custom');

function promisify(original) {
  if (typeof original !== 'function') {
    const err = new TypeError(
      `The "original" argument must be of type function. Received ${typeof original}`);
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  if (original[kCustomPromisifiedSymbol]) {
    const fn = original[kCustomPromisifiedSymbol];
    if (typeof fn !== 'function') {
      const err = new TypeError(
        'The "util.promisify.custom" property must be of type function.');
      err.code = 'ERR_INVALID_ARG_TYPE';
      throw err;
    }
    return Object.defineProperty(fn, kCustomPromisifiedSymbol, {
      value: fn, enumerable: false, writable: false, configurable: true,
    });
  }
  const argumentNames = original[customPromisifyArgs];
  function fn(...args) {
    return new Promise((resolve, reject) => {
      args.push((err, ...values) => {
        if (err) return reject(err);
        if (argumentNames !== undefined && values.length > 1) {
          const obj = {};
          for (let i = 0; i < argumentNames.length; i++) obj[argumentNames[i]] = values[i];
          resolve(obj);
        } else {
          resolve(values[0]);
        }
      });
      Reflect.apply(original, this, args);
    });
  }
  Object.setPrototypeOf(fn, Object.getPrototypeOf(original));
  Object.defineProperty(fn, kCustomPromisifiedSymbol, {
    value: fn, enumerable: false, writable: false, configurable: true,
  });
  const descriptors = Object.getOwnPropertyDescriptors(original);
  return Object.defineProperties(fn, descriptors);
}
promisify.custom = kCustomPromisifiedSymbol;

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

// `defineLazyProperties(target, 'module', [names])` — real lazy getters, not
// no-ops: `util.parseArgs` and friends resolve on first touch.
function defineLazyProperties(target, moduleName, names, needsEsmLoader) {
  for (const name of names) {
    Object.defineProperty(target, name, {
      configurable: true,
      enumerable: true,
      get() {
        const value = require(moduleName)[name];
        Object.defineProperty(target, name, {
          value, writable: true, configurable: true, enumerable: true,
        });
        return value;
      },
      set(value) {
        Object.defineProperty(target, name, {
          value, writable: true, configurable: true, enumerable: true,
        });
      },
    });
  }
}

function getLazy(initializer) {
  let value;
  let initialized = false;
  return () => {
    if (!initialized) {
      value = initializer();
      initialized = true;
    }
    return value;
  };
}

function spliceOne(list, index) {
  for (; index + 1 < list.length; index++) list[index] = list[index + 1];
  list.pop();
}

// eslint-disable-next-line no-control-regex
const colorRegExp = /\[\d\d?m/g;
function removeColors(str) {
  return String(str).replace(colorRegExp, '');
}

module.exports = {
  assignFunctionName,
  createDeferredPromise,
  customInspectSymbol,
  customPromisifyArgs,
  defineLazyProperties,
  deprecate,
  getLazy,
  kEmptyObject,
  kEnumerableProperty,
  once,
  promisify,
  removeColors,
  spliceOne,
  normalizeEncoding(enc) {
    if (enc == null || enc === 'utf8' || enc === 'utf-8') return 'utf8';
    return String(enc).toLowerCase();
  },
  getSystemErrorMap() { return new Map(); },
  convertProcessSignalToExitCode(signal) { return signal; },
  getInternalGlobal(name) { return globalThis[name]; },
  SideEffectFreeRegExpPrototypeExec: (re, str) => RegExp.prototype.exec.call(re, str),
  SideEffectFreeRegExpPrototypeSymbolReplace: (re, str, replacement) =>
    RegExp.prototype[Symbol.replace].call(re, str, replacement),
  isError(value) { return value instanceof Error; },
  emitExperimentalWarning() {},
  assertCrypto() {},
  join(output, separator) { return output.join(separator); },
  defineOperation(target, name, method) { target[name] = method; },
  exposeInterface() {},
  lazyDOMException(message, name) {
    const err = new Error(message);
    err.name = name;
    return err;
  },
};
