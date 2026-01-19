// Runtime bootstrap for module/builtin registration.
//
// Node.js builtins are registered via:
// - `__registerNodeBuiltin(nameOrNodeUrl, exports)`
// - `__registerNodeBuiltinLoader(nameOrNodeUrl, loaderFn)` (lazy)
//
// Otter builtins are registered via:
// - `__registerOtterBuiltin(nameOrOtterUrl, exports)`
// - `__registerOtterBuiltinLoader(nameOrOtterUrl, loaderFn)` (lazy)
(function (globalThis) {
  "use strict";

  // V8-compatible Error.prepareStackTrace and CallSite API for Node.js compatibility
  // Many packages like depd, source-map-support use this API
  if (!Error.prepareStackTrace) {
    // CallSite class - V8-compatible call site object
    class CallSite {
      constructor(info) {
        this._functionName = info.functionName || null;
        this._fileName = info.fileName || null;
        this._lineNumber = info.lineNumber || null;
        this._columnNumber = info.columnNumber || null;
        this._isNative = info.isNative || false;
        this._isEval = info.isEval || false;
        this._isConstructor = info.isConstructor || false;
        this._isToplevel = info.isToplevel || false;
        this._typeName = info.typeName || null;
        this._methodName = info.methodName || null;
      }
      getFileName() { return this._fileName; }
      getLineNumber() { return this._lineNumber; }
      getColumnNumber() { return this._columnNumber; }
      getFunctionName() { return this._functionName; }
      getTypeName() { return this._typeName; }
      getMethodName() { return this._methodName; }
      isNative() { return this._isNative; }
      isEval() { return this._isEval; }
      isConstructor() { return this._isConstructor; }
      isToplevel() { return this._isToplevel; }
      getEvalOrigin() { return null; }
      getThis() { return undefined; }
      getFunction() { return undefined; }
      toString() {
        let str = '';
        if (this._functionName) {
          str += this._functionName;
        } else {
          str += '<anonymous>';
        }
        if (this._fileName) {
          str += ' (' + this._fileName;
          if (this._lineNumber != null) {
            str += ':' + this._lineNumber;
            if (this._columnNumber != null) {
              str += ':' + this._columnNumber;
            }
          }
          str += ')';
        }
        return str;
      }
    }

    // Parse JSC stack trace into CallSite array
    function parseStackTrace(stack) {
      if (!stack || typeof stack !== 'string') return [];
      const lines = stack.split('\n');
      const callSites = [];

      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed || trimmed.startsWith('Error')) continue;

        // JSC format: "functionName@fileName:line:column" or "@fileName:line:column" for anonymous
        // Also handles: "    at functionName (fileName:line:column)"
        let match;

        // V8 format: "    at functionName (fileName:line:column)"
        match = trimmed.match(/^at\s+(?:(.+?)\s+)?\(?([^:]+):(\d+):(\d+)\)?$/);
        if (match) {
          callSites.push(new CallSite({
            functionName: match[1] || null,
            fileName: match[2],
            lineNumber: parseInt(match[3], 10),
            columnNumber: parseInt(match[4], 10),
            isNative: match[2] === 'native',
          }));
          continue;
        }

        // JSC format: "functionName@fileName:line:column"
        match = trimmed.match(/^(.*)@([^@]+):(\d+):(\d+)$/);
        if (match) {
          callSites.push(new CallSite({
            functionName: match[1] || null,
            fileName: match[2],
            lineNumber: parseInt(match[3], 10),
            columnNumber: parseInt(match[4], 10),
            isNative: match[2] === '[native code]',
          }));
          continue;
        }

        // Simple format: "functionName@fileName:line"
        match = trimmed.match(/^(.*)@([^@]+):(\d+)$/);
        if (match) {
          callSites.push(new CallSite({
            functionName: match[1] || null,
            fileName: match[2],
            lineNumber: parseInt(match[3], 10),
            columnNumber: null,
          }));
          continue;
        }

        // Native code: "[native code]"
        if (trimmed.includes('[native code]')) {
          const fnMatch = trimmed.match(/^(.*)@\[native code\]$/);
          callSites.push(new CallSite({
            functionName: fnMatch ? fnMatch[1] : null,
            isNative: true,
          }));
          continue;
        }
      }

      return callSites;
    }

    // Store original stack descriptor
    const errorProto = Error.prototype;
    const originalStackGetter = Object.getOwnPropertyDescriptor(errorProto, 'stack')?.get;

    // Set up captureStackTrace (V8 API)
    Error.captureStackTrace = function(targetObject, constructorOpt) {
      const tempError = new Error();
      const stack = tempError.stack;

      if (Error.prepareStackTrace) {
        const callSites = parseStackTrace(stack);
        // Skip frames until we find constructorOpt, or skip first frame
        let skipCount = 1; // Skip captureStackTrace itself
        if (constructorOpt) {
          for (let i = 0; i < callSites.length; i++) {
            if (callSites[i].getFunctionName() === constructorOpt.name) {
              skipCount = i + 1;
              break;
            }
          }
        }
        const filteredCallSites = callSites.slice(skipCount);
        try {
          targetObject.stack = Error.prepareStackTrace(targetObject, filteredCallSites);
        } catch (e) {
          targetObject.stack = stack;
        }
      } else {
        targetObject.stack = stack;
      }
    };

    // Default prepareStackTrace - can be overridden by user code
    Error.prepareStackTrace = function(error, callSites) {
      let result = error.name || 'Error';
      if (error.message) {
        result += ': ' + error.message;
      }
      for (const site of callSites) {
        result += '\n    at ' + site.toString();
      }
      return result;
    };

    // Expose CallSite class for testing/debugging
    globalThis.__CallSite = CallSite;
    globalThis.__parseStackTrace = parseStackTrace;
  }

  function __otter_define_immutable_global(name, value) {
    const desc = Object.getOwnPropertyDescriptor(globalThis, name);
    if (desc && desc.configurable === false) {
      return;
    }
    Object.defineProperty(globalThis, name, {
      value,
      writable: false,
      configurable: false,
      enumerable: false,
    });
  }

  // ---------------------------------------------------------------------------
  // Built-in module registries (strict allowlist + helpful runtime errors)
  // ---------------------------------------------------------------------------

  const __otter_node_builtins = Object.create(null);
  const __otter_node_builtin_loaders = Object.create(null);
  const __otter_otter_builtins = Object.create(null);
  const __otter_otter_builtin_loaders = Object.create(null);
  let __otter_builtins_locked = false;

  const __otter_node_builtin_names = Array.isArray(globalThis.__otter_node_builtin_names)
    ? globalThis.__otter_node_builtin_names
    : [];
  const __otter_node_builtin_set = new Set(__otter_node_builtin_names);

  const __otter_otter_builtin_set = new Set(["otter"]);

  function __otter_normalize_node_builtin(specifier) {
    specifier = String(specifier);
    return specifier.startsWith("node:") ? specifier.slice(5) : specifier;
  }

  function __otter_is_node_builtin(specifier) {
    return __otter_node_builtin_set.has(__otter_normalize_node_builtin(specifier));
  }

  function __otter_normalize_otter_builtin(specifier) {
    specifier = String(specifier);
    return specifier.startsWith("otter:") ? specifier.slice(6) : specifier;
  }

  function __otter_is_otter_builtin(specifier) {
    return __otter_otter_builtin_set.has(__otter_normalize_otter_builtin(specifier));
  }

  if (typeof globalThis.__otter_is_node_builtin !== "function") {
    __otter_define_immutable_global("__otter_is_node_builtin", __otter_is_node_builtin);
  }

  if (typeof globalThis.__otter_is_otter_builtin !== "function") {
    __otter_define_immutable_global("__otter_is_otter_builtin", __otter_is_otter_builtin);
  }

  if (typeof globalThis.__otter_lock_builtins !== "function") {
    __otter_define_immutable_global("__otter_lock_builtins", function __otter_lock_builtins() {
      __otter_builtins_locked = true;
    });
  }

  // Register a Node.js builtin (either "fs" or "node:fs").
  if (typeof globalThis.__registerNodeBuiltin !== "function") {
    __otter_define_immutable_global("__registerNodeBuiltin", function __registerNodeBuiltin(specifier, exports) {
      const name = __otter_normalize_node_builtin(specifier);
      if (!__otter_node_builtin_set.has(name)) {
        throw new Error(
          `Refusing to register unsupported Node.js builtin: ${String(specifier)}`
        );
      }
      if (__otter_builtins_locked) {
        throw new Error(`Node.js builtin registration is locked: ${String(specifier)}`);
      }
      if (Object.prototype.hasOwnProperty.call(__otter_node_builtins, name)) {
        throw new Error(`Node.js builtin already registered: ${String(specifier)}`);
      }
      __otter_node_builtins[name] = exports;
      return exports;
    });
  }

  // Register a lazy loader for a Node.js builtin.
  if (typeof globalThis.__registerNodeBuiltinLoader !== "function") {
    __otter_define_immutable_global("__registerNodeBuiltinLoader", function __registerNodeBuiltinLoader(specifier, loaderFn) {
      const name = __otter_normalize_node_builtin(specifier);
      if (!__otter_node_builtin_set.has(name)) {
        throw new Error(
          `Refusing to register loader for unsupported Node.js builtin: ${String(specifier)}`
        );
      }
      if (__otter_builtins_locked) {
        throw new Error(`Node.js builtin loader registration is locked: ${String(specifier)}`);
      }
      if (typeof loaderFn !== "function") {
        throw new Error(`Loader for ${String(specifier)} must be a function`);
      }
      if (Object.prototype.hasOwnProperty.call(__otter_node_builtins, name)) {
        throw new Error(`Node.js builtin already registered: ${String(specifier)}`);
      }
      if (Object.prototype.hasOwnProperty.call(__otter_node_builtin_loaders, name)) {
        throw new Error(`Node.js builtin loader already registered: ${String(specifier)}`);
      }
      __otter_node_builtin_loaders[name] = loaderFn;
    });
  }

  // Get a Node.js builtin module. Throws a clear error if it's missing.
  if (typeof globalThis.__otter_get_node_builtin !== "function") {
    __otter_define_immutable_global("__otter_get_node_builtin", function __otter_get_node_builtin(specifier) {
      const raw = String(specifier);
      const name = __otter_normalize_node_builtin(raw);

      if (!__otter_node_builtin_set.has(name)) {
        throw new Error(
          `Unknown Node.js builtin module: ${raw}\n` +
          `Only real node:* builtins are allowed.`
        );
      }

      if (Object.prototype.hasOwnProperty.call(__otter_node_builtins, name)) {
        return __otter_node_builtins[name];
      }

      const loader = __otter_node_builtin_loaders[name];
      if (typeof loader === "function") {
        const exports = loader();
        globalThis.__registerNodeBuiltin(name, exports);
        delete __otter_node_builtin_loaders[name];
        return exports;
      }

      throw new Error(
        `Node.js builtin module ${raw} is not available in this runtime.\n` +
        `The host likely did not register the corresponding extension.\n` +
        `If you use otter-node, register Node compatibility extensions before running user code.`
      );
    });
  }

  // Register an Otter builtin (either "otter" or "otter:otter").
  // Supports additive registration - if already registered, merges exports.
  if (typeof globalThis.__registerOtterBuiltin !== "function") {
    __otter_define_immutable_global("__registerOtterBuiltin", function __registerOtterBuiltin(specifier, exports) {
      const name = __otter_normalize_otter_builtin(specifier);
      if (!__otter_otter_builtin_set.has(name)) {
        throw new Error(
          `Refusing to register unsupported otter builtin: ${String(specifier)}`
        );
      }
      if (__otter_builtins_locked) {
        throw new Error(`Otter builtin registration is locked: ${String(specifier)}`);
      }
      // Support additive registration - merge with existing exports
      if (Object.prototype.hasOwnProperty.call(__otter_otter_builtins, name)) {
        __otter_otter_builtins[name] = { ...__otter_otter_builtins[name], ...exports };
      } else {
        __otter_otter_builtins[name] = exports;
      }
      return __otter_otter_builtins[name];
    });
  }

  // Register a lazy loader for an Otter builtin.
  if (typeof globalThis.__registerOtterBuiltinLoader !== "function") {
    __otter_define_immutable_global("__registerOtterBuiltinLoader", function __registerOtterBuiltinLoader(specifier, loaderFn) {
      const name = __otter_normalize_otter_builtin(specifier);
      if (!__otter_otter_builtin_set.has(name)) {
        throw new Error(
          `Refusing to register loader for unsupported otter builtin: ${String(specifier)}`
        );
      }
      if (__otter_builtins_locked) {
        throw new Error(`Otter builtin loader registration is locked: ${String(specifier)}`);
      }
      if (typeof loaderFn !== "function") {
        throw new Error(`Loader for ${String(specifier)} must be a function`);
      }
      if (Object.prototype.hasOwnProperty.call(__otter_otter_builtins, name)) {
        throw new Error(`Otter builtin already registered: ${String(specifier)}`);
      }
      if (Object.prototype.hasOwnProperty.call(__otter_otter_builtin_loaders, name)) {
        throw new Error(`Otter builtin loader already registered: ${String(specifier)}`);
      }
      __otter_otter_builtin_loaders[name] = loaderFn;
    });
  }

  // Get an Otter builtin module. Throws a clear error if it's missing.
  if (typeof globalThis.__otter_get_otter_builtin !== "function") {
    __otter_define_immutable_global("__otter_get_otter_builtin", function __otter_get_otter_builtin(specifier) {
      const raw = String(specifier);
      const name = __otter_normalize_otter_builtin(raw);

      if (!__otter_otter_builtin_set.has(name)) {
        throw new Error(`Unknown otter builtin module: ${raw}`);
      }

      if (Object.prototype.hasOwnProperty.call(__otter_otter_builtins, name)) {
        return __otter_otter_builtins[name];
      }

      const loader = __otter_otter_builtin_loaders[name];
      if (typeof loader === "function") {
        const exports = loader();
        globalThis.__registerOtterBuiltin(name, exports);
        delete __otter_otter_builtin_loaders[name];
        return exports;
      }

      throw new Error(
        `Otter builtin module ${raw} is not available in this runtime.\n` +
        `The host likely did not register the corresponding extension.`
      );
    });
  }

  if (typeof globalThis.__otter_peek_otter_builtin !== "function") {
    __otter_define_immutable_global("__otter_peek_otter_builtin", function __otter_peek_otter_builtin(specifier) {
      const raw = String(specifier);
      const name = __otter_normalize_otter_builtin(raw);
      if (!__otter_otter_builtin_set.has(name)) {
        return undefined;
      }
      return __otter_otter_builtins[name];
    });
  }

  // Dynamic import runtime function for variable-based imports
  // Handles: import(variableName) or import(expression)
  if (typeof globalThis.__otter_dynamic_import !== "function") {
    globalThis.__otter_dynamic_import = async function __otter_dynamic_import(specifier) {
      // Ensure specifier is a string
      specifier = String(specifier);

      // Check pre-bundled ESM modules (already wrapped if needed)
      if (globalThis.__otter_modules && globalThis.__otter_modules[specifier]) {
        return globalThis.__otter_modules[specifier];
      }

      // Check pre-bundled CJS modules - wrap with __toESM for ESM consumption
      if (globalThis.__otter_cjs_modules?.[specifier]) {
        const cjsMod = globalThis.__otter_cjs_modules[specifier]();
        // Wrap CJS module for ESM consumption
        const esmMod = typeof __toESM === "function" ? __toESM(cjsMod, 1) : cjsMod;
        // Cache the wrapped module for future imports
        globalThis.__otter_modules = globalThis.__otter_modules || {};
        globalThis.__otter_modules[specifier] = esmMod;
        return esmMod;
      }

      // Check Node.js builtins
      if (typeof globalThis.__otter_is_node_builtin === "function" && globalThis.__otter_is_node_builtin(specifier)) {
        return globalThis.__otter_get_node_builtin(specifier);
      }

      // Check Otter builtins
      if (typeof globalThis.__otter_is_otter_builtin === "function" && globalThis.__otter_is_otter_builtin(specifier)) {
        return globalThis.__otter_get_otter_builtin(specifier);
      }

      // Try loading via native op (if available)
      if (typeof globalThis.__otter_load_module === "function") {
        return await globalThis.__otter_load_module(specifier);
      }

      // Try CommonJS require as fallback
      if (typeof require === "function") {
        try {
          return require(specifier);
        } catch (e) {
          // Fall through to error
        }
      }

      throw new Error(`Cannot dynamically import module: ${specifier}`);
    };
  }
})(globalThis);
