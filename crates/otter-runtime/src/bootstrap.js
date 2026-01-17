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
})(globalThis);

