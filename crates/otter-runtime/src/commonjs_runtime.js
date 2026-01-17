// CommonJS runtime helpers for ESM/CJS interop
// These helpers enable seamless interoperability between ESM and CommonJS modules

(function(globalThis) {
    "use strict";

    var __create = Object.create;
    var __defProp = Object.defineProperty;
    var __getOwnPropNames = Object.getOwnPropertyNames;
    var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
    var __getProtoOf = Object.getPrototypeOf;
    var __hasOwnProp = Object.prototype.hasOwnProperty;

    // Cache for __toCommonJS results
    var __moduleCache = new WeakMap();

    // Lazy CommonJS wrapper with caching
    // Usage: var require_foo = __commonJS((exports, module) => { ... });
    // Then: require_foo() returns module.exports
    globalThis.__commonJS = function(cb, mod) {
        return function() {
            if (mod) return mod.exports;
            mod = { exports: {} };
            cb(mod.exports, mod);
            return mod.exports;
        };
    };

    // Convert CommonJS module to ESM format
    // Creates an object with 'default' pointing to module.exports
    // and copies named exports if the module has them
    globalThis.__toESM = function(mod, isNodeMode) {
        if (mod && mod.__esModule) {
            return mod;
        }

        var target = mod != null ? __create(__getProtoOf(mod)) : {};

        // If not a module or in Node compatibility mode, set default to the whole module
        if (isNodeMode || !mod || !mod.__esModule) {
            __defProp(target, "default", { value: mod, enumerable: true });
        }

        // Copy all properties with getters for live bindings
        if (mod != null) {
            for (var key of __getOwnPropNames(mod)) {
                if (!__hasOwnProp.call(target, key)) {
                    __defProp(target, key, {
                        get: (function(k) { return function() { return mod[k]; }; })(key),
                        enumerable: true
                    });
                }
            }
        }

        return target;
    };

    // Convert ESM module to CommonJS format
    // Adds __esModule: true and copies all exports with getters
    globalThis.__toCommonJS = function(from) {
        var entry = __moduleCache.get(from);
        if (entry) return entry;

        entry = __defProp({}, "__esModule", { value: true });

        if ((from && typeof from === "object") || typeof from === "function") {
            for (var key of __getOwnPropNames(from)) {
                if (!__hasOwnProp.call(entry, key)) {
                    var desc = __getOwnPropDesc(from, key);
                    __defProp(entry, key, {
                        get: (function(k) { return function() { return from[k]; }; })(key),
                        enumerable: !(desc) || desc.enumerable
                    });
                }
            }
        }

        __moduleCache.set(from, entry);
        return entry;
    };

    // Re-export helper for "export * from" statements
    globalThis.__reExport = function(target, mod, secondTarget) {
        for (var key of __getOwnPropNames(mod)) {
            if (!__hasOwnProp.call(target, key) && key !== "default") {
                __defProp(target, key, {
                    get: (function(k) { return function() { return mod[k]; }; })(key),
                    enumerable: true
                });
            }
        }

        if (secondTarget) {
            for (var key of __getOwnPropNames(mod)) {
                if (!__hasOwnProp.call(secondTarget, key) && key !== "default") {
                    __defProp(secondTarget, key, {
                        get: (function(k) { return function() { return mod[k]; }; })(key),
                        enumerable: true
                    });
                }
            }
        }

        return secondTarget || target;
    };

    // Export helper for ESM to CJS conversion
    globalThis.__export = function(target, all) {
        for (var name in all) {
            __defProp(target, name, {
                get: all[name],
                enumerable: true,
                configurable: true,
                set: (function(n) {
                    return function(newValue) { all[n] = function() { return newValue; }; };
                })(name)
            });
        }
    };

    // Export value helper (simpler version for static exports)
    globalThis.__exportValue = function(target, all) {
        for (var name in all) {
            __defProp(target, name, {
                get: (function(n) { return function() { return all[n]; }; })(name),
                set: (function(n) { return function(newValue) { all[n] = newValue; }; })(name),
                enumerable: true,
                configurable: true
            });
        }
    };

    // Export default helper
    globalThis.__exportDefault = function(target, value) {
        __defProp(target, "default", {
            get: function() { return value; },
            set: function(newValue) { value = newValue; },
            enumerable: true,
            configurable: true
        });
    };

    // CommonJS module registry (separate from ESM registry)
    globalThis.__otter_cjs_modules = globalThis.__otter_cjs_modules || {};

    // Create require function for a specific module context
    globalThis.__createRequire = function(dirname, filename) {
        var require = function(specifier) {
            // Check if it's a node: built-in
            if (specifier.startsWith("node:")) {
                var builtinName = specifier.slice(5);
                if (globalThis.__otter_node_builtins && globalThis.__otter_node_builtins[builtinName]) {
                    return globalThis.__otter_node_builtins[builtinName];
                }
                throw new Error("Cannot find module '" + specifier + "'");
            }

            // Check bare builtin (without node: prefix)
            if (globalThis.__otter_node_builtins && globalThis.__otter_node_builtins[specifier]) {
                return globalThis.__otter_node_builtins[specifier];
            }

            // Check CJS module registry
            if (globalThis.__otter_cjs_modules[specifier]) {
                return globalThis.__otter_cjs_modules[specifier]();
            }

            // Check ESM module registry (with conversion)
            if (globalThis.__otter_modules && globalThis.__otter_modules[specifier]) {
                return globalThis.__toCommonJS(globalThis.__otter_modules[specifier]);
            }

            throw new Error("Cannot find module '" + specifier + "' from '" + dirname + "'");
        };

        require.resolve = function(specifier) {
            // For now, just return the specifier
            // Full resolution would need the module loader
            return specifier;
        };

        require.cache = globalThis.__otter_cjs_modules;
        require.main = undefined;

        return require;
    };

})(globalThis);
