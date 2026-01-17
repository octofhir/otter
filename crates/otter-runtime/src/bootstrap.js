// Runtime bootstrap for module/builtin registration.
//
// Extensions can call `__registerModule(name, exports)` to populate the
// built-in module table used by CommonJS `require()` and the bundler.
(function (globalThis) {
  "use strict";

  if (!globalThis.__otter_node_builtins) {
    globalThis.__otter_node_builtins = Object.create(null);
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
})(globalThis);

