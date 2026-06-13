'use strict';
// node:vm — a best-effort, in-realm sandbox. True realm isolation needs engine
// support (a separate global object + completion-value eval); this shim runs
// code inside a `with`-scoped Proxy so the sandbox's own properties seed the
// run scope and undeclared assignments land back on the sandbox. JS intrinsics
// (Object, Array, JSON, ...) fall through to the host realm; Node host globals
// (process, Buffer, require, ...) are hidden so a bare context looks like one.

// Host globals a fresh vm context must NOT inherit (it has JS intrinsics but
// not the Node environment).
const hiddenGlobals = new Set([
  'process', 'Buffer', 'require', 'module', 'exports', '__dirname', '__filename',
  'global', 'globalThis', 'console', 'setTimeout', 'setInterval', 'setImmediate',
  'clearTimeout', 'clearInterval', 'clearImmediate', 'queueMicrotask',
]);

const contexts = new WeakSet();

function argTypeError(name, expected, value) {
  let received;
  if (value === null || value === undefined) received = ` Received ${value}`;
  else received = ` Received type ${typeof value} (${typeof value === 'string' ? `'${value}'` : String(value)})`;
  const e = new TypeError(`The "${name}" argument must be ${expected}.${received}`);
  e.code = 'ERR_INVALID_ARG_TYPE';
  return e;
}

function makeSandboxProxy(sandbox) {
  return new Proxy(sandbox, {
    // Claim every identifier so `with` routes all reads/writes through here.
    has() { return true; },
    get(target, key) {
      if (key === Symbol.unscopables) return undefined;
      if (key in target) return target[key];
      if (typeof key === 'string' && hiddenGlobals.has(key)) return undefined;
      return globalThis[key];
    },
    set(target, key, value) {
      target[key] = value;
      return true;
    },
  });
}

// Compile `code` into a function evaluated with the proxy as its `with` scope.
// Single-expression scripts return their value (vm reports the completion
// value); statement lists run for side effects and return undefined.
function compileInScope(code) {
  const stripped = String(code).replace(/;\s*$/, '');
  let body;
  try {
    body = new Function('__scope__', `with (__scope__) { return (\n${stripped}\n); }`);
  } catch {
    body = new Function('__scope__', `with (__scope__) {\n${code}\n}`);
  }
  return body;
}

function runWith(code, sandbox) {
  const proxy = makeSandboxProxy(sandbox);
  const fn = compileInScope(code);
  return fn.call(sandbox, proxy);
}

function createContext(sandbox, _options) {
  if (sandbox === undefined) sandbox = {};
  if (typeof sandbox !== 'object' || sandbox === null) {
    throw argTypeError('contextObject', 'an instance of Object', sandbox);
  }
  contexts.add(sandbox);
  return sandbox;
}

function isContext(sandbox) {
  if (typeof sandbox !== 'object' || sandbox === null) {
    throw argTypeError('contextifiedObject', 'an instance of Object', sandbox);
  }
  return contexts.has(sandbox);
}

function runInNewContext(code, sandbox = {}, _options) {
  createContext(sandbox);
  return runWith(code, sandbox);
}

function runInContext(code, context, _options) {
  return runWith(code, context);
}

function runInThisContext(code, _options) {
  // Indirect eval — runs in the host global scope.
  return (0, eval)(String(code)); // eslint-disable-line no-eval
}

function compileFunction(code, params = [], _options) {
  return new Function(...params, String(code));
}

class Script {
  constructor(code, options) {
    this.code = String(code);
    // Code caching is not implemented; accept any supplied cachedData as
    // valid (never rejected) and hand back an opaque buffer on request.
    this.cachedDataRejected = options && options.cachedData ? false : undefined;
    this.cachedDataProduced = false;
  }

  createCachedData() {
    this.cachedDataProduced = true;
    return Buffer.from([]);
  }

  runInThisContext(_options) {
    return runInThisContext(this.code);
  }

  runInContext(context, _options) {
    return runWith(this.code, context);
  }

  runInNewContext(sandbox = {}, _options) {
    createContext(sandbox);
    return runWith(this.code, sandbox);
  }
}

// vm.constants — a stub mirror of Node's surface.
const constants = Object.freeze({
  DONT_CONTEXTIFY: Symbol('vm_dont_contextify'),
  USE_MAIN_CONTEXT_DEFAULT_LOADER: 0,
});

module.exports = {
  createContext,
  isContext,
  runInNewContext,
  runInContext,
  runInThisContext,
  compileFunction,
  Script,
  constants,
  measureMemory: () => Promise.resolve({ total: { jsMemoryEstimate: 0, jsMemoryRange: [0, 0] } }),
};
