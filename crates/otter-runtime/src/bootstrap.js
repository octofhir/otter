// Runtime bootstrap for module/builtin registration.
//
// Extensions can call `__registerModule(name, exports)` to populate the
// built-in module table used by CommonJS `require()` and the bundler.
//
// For lazy loading, use `__registerModuleLoader(name, loaderFn)` - the loader
// is called only when the module is first required.
(function (globalThis) {
  "use strict";

  if (!globalThis.__otter_node_builtins) {
    globalThis.__otter_node_builtins = Object.create(null);
  }

  // Lazy module loaders - called on first require
  if (!globalThis.__otter_module_loaders) {
    globalThis.__otter_module_loaders = Object.create(null);
  }

  if (typeof globalThis.__registerModule !== "function") {
    globalThis.__registerModule = function __registerModule(name, exports) {
      globalThis.__otter_node_builtins[name] = exports;

      if (typeof name === "string") {
        if (name.startsWith("node:")) {
          globalThis.__otter_node_builtins[name.slice(5)] = exports;
        } else {
          globalThis.__otter_node_builtins["node:" + name] = exports;
        }
      }

      return exports;
    };
  }

  // Register a lazy loader - module code runs only when first required
  if (typeof globalThis.__registerModuleLoader !== "function") {
    globalThis.__registerModuleLoader = function __registerModuleLoader(name, loaderFn) {
      globalThis.__otter_module_loaders[name] = loaderFn;

      if (typeof name === "string") {
        if (name.startsWith("node:")) {
          globalThis.__otter_module_loaders[name.slice(5)] = loaderFn;
        } else {
          globalThis.__otter_module_loaders["node:" + name] = loaderFn;
        }
      }
    };
  }

  // Get module - loads lazily if needed
  if (typeof globalThis.__getModule !== "function") {
    globalThis.__getModule = function __getModule(name) {
      // Already loaded?
      if (globalThis.__otter_node_builtins[name]) {
        return globalThis.__otter_node_builtins[name];
      }

      // Has lazy loader?
      const loader = globalThis.__otter_module_loaders[name];
      if (loader) {
        const exports = loader();
        globalThis.__registerModule(name, exports);
        delete globalThis.__otter_module_loaders[name];
        // Clean up alternate name too
        if (name.startsWith("node:")) {
          delete globalThis.__otter_module_loaders[name.slice(5)];
        } else {
          delete globalThis.__otter_module_loaders["node:" + name];
        }
        return exports;
      }

      return undefined;
    };
  }

  // Dynamic import runtime function for variable-based imports
  // Handles: import(variableName) or import(expression)
  if (typeof globalThis.__otter_dynamic_import !== "function") {
    globalThis.__otter_dynamic_import = async function __otter_dynamic_import(specifier) {
      // Ensure specifier is a string
      specifier = String(specifier);

      // Check pre-bundled ESM modules
      if (globalThis.__otter_modules && globalThis.__otter_modules[specifier]) {
        return globalThis.__otter_modules[specifier];
      }

      // Check Node.js builtins
      const builtinName = specifier.startsWith("node:") ? specifier.slice(5) : specifier;
      if (globalThis.__otter_node_builtins && globalThis.__otter_node_builtins[builtinName]) {
        return globalThis.__otter_node_builtins[builtinName];
      }

      // Try lazy module loader
      const mod = globalThis.__getModule(specifier);
      if (mod) {
        return mod;
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

