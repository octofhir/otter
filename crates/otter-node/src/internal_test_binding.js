'use strict';
// `internal/test/binding` — test-harness hooks used by selected Node
// compatibility fixtures. This is not a public Node API.

const STORE_KEY = '__otterInternalTestBinding';
const store = globalThis[STORE_KEY] || Object.defineProperty(globalThis, STORE_KEY, {
  value: { __proto__: null },
  configurable: true,
}).__otterInternalTestBinding;

function osBinding() {
  const binding = store.os || (store.os = { __proto__: null });

  if (!binding.__otterGetHomeDirectoryAccessor) {
    Object.defineProperty(binding, 'getHomeDirectory', {
      configurable: true,
      enumerable: true,
      get() {
        return binding.__otterGetHomeDirectory;
      },
      set(fn) {
        binding.__otterGetHomeDirectory = fn;
        binding.getHomeDirectoryError = undefined;
        if (typeof fn !== 'function') return;
        const ctx = { __proto__: null };
        fn(ctx);
        if (ctx.syscall !== undefined || ctx.code !== undefined || ctx.message !== undefined) {
          binding.getHomeDirectoryError = {
            syscall: String(ctx.syscall),
            code: String(ctx.code),
            message: String(ctx.message),
          };
        }
      },
    });
    Object.defineProperty(binding, '__otterGetHomeDirectoryAccessor', {
      value: true,
      configurable: true,
    });
  }

  return binding;
}

function debugBinding() {
  if (!store.debug) {
    store.debug = {
      __proto__: null,
      getV8FastApiCallCount() {
        return 0;
      },
    };
  }
  return store.debug;
}

function internalBinding(name) {
  switch (String(name)) {
    case 'os':
      return osBinding();
    case 'debug':
      return debugBinding();
    default:
      return {};
  }
}

module.exports = { internalBinding };
