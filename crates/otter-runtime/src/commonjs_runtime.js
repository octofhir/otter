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

    // Lazy CommonJS wrapper with caching and thunk optimization
    // Usage: var require_foo = __commonJS((exports, module) => { ... });
    // Then: require_foo() returns module.exports
    //
    // Optimization: After first execution, replace the thunk with a simple getter
    // to avoid the closure overhead on subsequent calls (like Bun does)
    globalThis.__commonJS = function(cb, mod) {
        var fn = function __cjs_thunk() {
            if (mod) return mod.exports;
            mod = { exports: {} };
            cb(mod.exports, mod);
            // Replace thunk with simple getter after initialization
            fn = function __cjs_getter() { return mod.exports; };
            return mod.exports;
        };
        // Return stable wrapper that delegates to fn
        // This allows fn to be replaced while maintaining the same reference
        return function __cjs_wrapper() { return fn(); };
    };

    // Convert CommonJS module to ESM format
    // Creates an object with 'default' pointing to module.exports
    // and copies named exports if the module has them
    // CRITICAL: Preserves callable nature of functions (e.g., axios)
    globalThis.__toESM = function(mod, isNodeMode) {
        if (mod && mod.__esModule) {
            return mod;
        }

        var target;

        // CRITICAL: Preserve callable nature for function exports (axios, express, etc.)
        if (typeof mod === "function") {
            // Create callable wrapper that delegates to original function
            target = function __esm_callable_wrapper() {
                return mod.apply(this, arguments);
            };
            // Preserve prototype for instanceof checks
            target.prototype = mod.prototype;
            // Preserve prototype chain
            Object.setPrototypeOf(target, Object.getPrototypeOf(mod));
        } else {
            target = mod != null ? __create(__getProtoOf(mod)) : {};
        }

        // If not a module or in Node compatibility mode, set default to the whole module
        if (isNodeMode || !mod || !mod.__esModule) {
            __defProp(target, "default", { value: mod, enumerable: true });
        }

        // Copy all properties with getters for live bindings
        // Skip 'prototype' for functions as it's already set above
        if (mod != null) {
            for (var key of __getOwnPropNames(mod)) {
                if (!__hasOwnProp.call(target, key) && key !== "default" && key !== "prototype") {
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

    // Resolve a relative path against a base directory
    function resolvePath(specifier, dirname) {
        if (!specifier.startsWith('./') && !specifier.startsWith('../')) {
            return specifier;  // Not a relative path
        }

        // Normalize dirname to remove any trailing slash
        var base = dirname.endsWith('/') ? dirname.slice(0, -1) : dirname;
        var parts = base.split('/');
        var specParts = specifier.split('/');

        for (var i = 0; i < specParts.length; i++) {
            var part = specParts[i];
            if (part === '.' || part === '') {
                continue;
            } else if (part === '..') {
                parts.pop();
            } else {
                parts.push(part);
            }
        }

        return parts.join('/');
    }

    // JSON module cache
    var __jsonCache = {};

    // File module cache (for dynamically loaded .js files)
    var __fileModuleCache = {};

    // Helper to resolve file path with extension resolution
    function resolveFilePath(basePath, fs) {
        var extensions = ['', '.js', '.cjs', '/index.js', '/index.cjs'];
        for (var i = 0; i < extensions.length; i++) {
            var tryPath = basePath + extensions[i];
            try {
                // Check if file exists using fs.statSync or readFileSync
                if (fs.existsSync && fs.existsSync(tryPath)) {
                    // Check if it's a directory, if so try index.js
                    var stat = fs.statSync(tryPath);
                    if (stat.isDirectory && stat.isDirectory()) {
                        var indexPath = tryPath + '/index.js';
                        if (fs.existsSync(indexPath)) {
                            return indexPath;
                        }
                        continue;
                    }
                    return tryPath;
                }
            } catch (e) {
                // File doesn't exist, try next extension
            }
        }
        return null;
    }

    // Load and execute a CommonJS module from disk
    function loadFileModule(filePath, fs) {
        // Check cache first
        if (__fileModuleCache[filePath]) {
            return __fileModuleCache[filePath].exports;
        }

        // Read file content
        var content;
        try {
            content = fs.readFileSync(filePath, 'utf8');
        } catch (e) {
            return null;
        }

        // Extract dirname and filename
        var lastSlash = filePath.lastIndexOf('/');
        var moduleDirname = lastSlash > 0 ? filePath.slice(0, lastSlash) : '/';
        var moduleFilename = filePath;

        // Create module object
        var module = {
            id: filePath,
            filename: filePath,
            loaded: false,
            exports: {},
            paths: [],
            children: []
        };

        // Cache before executing (handles circular dependencies)
        __fileModuleCache[filePath] = module;

        // Create require function for this module
        var moduleRequire = globalThis.__createRequire(moduleDirname, moduleFilename);

        // Wrap the module code
        var wrappedCode = '(function(exports, require, module, __filename, __dirname) {\n' +
            content +
            '\n});';

        try {
            // Execute the wrapper to get the function
            var moduleFunc = (0, eval)(wrappedCode);

            // Execute the module
            moduleFunc.call(
                module.exports,
                module.exports,
                moduleRequire,
                module,
                moduleFilename,
                moduleDirname
            );

            module.loaded = true;
        } catch (e) {
            // Remove from cache on error
            delete __fileModuleCache[filePath];
            throw e;
        }

        return module.exports;
    }

    // Create require function for a specific module context
    // deps: optional map from specifier to resolved URL (passed by bundler for pre-resolved deps)
    globalThis.__createRequire = function(dirname, filename, deps) {
        deps = deps || {};

        // Normalize dirname to absolute path if possible
        var cwd = globalThis.process && globalThis.process.cwd ? globalThis.process.cwd() : '';
        var effectiveDirname = dirname;
        if (dirname === '.' || dirname === '') {
            effectiveDirname = cwd || '.';
        } else if (!dirname.startsWith('/') && cwd) {
            effectiveDirname = resolvePath('./' + dirname, cwd);
        }

        var require = function(specifier) {
            var resolvedFromDeps, resolved, absolutePath, mod;

            // Node.js builtins (strict allowlist + helpful errors)
            if (globalThis.__otter_is_node_builtin && globalThis.__otter_is_node_builtin(specifier)) {
                return globalThis.__otter_get_node_builtin(specifier);
            }

            // Otter builtins (e.g. "otter")
            if (globalThis.__otter_is_otter_builtin && globalThis.__otter_is_otter_builtin(specifier)) {
                return globalThis.__otter_get_otter_builtin(specifier);
            }

            // First, check if we have a pre-resolved dependency from the bundler
            // This handles bare specifiers like 'combined-stream' that were resolved at bundle time
            if (deps[specifier]) {
                resolvedFromDeps = deps[specifier];
                // Try CJS module first (most common for npm packages)
                if (globalThis.__otter_cjs_modules[resolvedFromDeps]) {
                    return globalThis.__otter_cjs_modules[resolvedFromDeps]();
                }
                // Try ESM module (convert to CJS)
                if (globalThis.__otter_modules?.[resolvedFromDeps]) {
                    return globalThis.__toCommonJS(globalThis.__otter_modules[resolvedFromDeps]);
                }
            }

            // Resolve relative paths to absolute paths
            resolved = specifier;
            absolutePath = null;
            if (specifier.startsWith('./') || specifier.startsWith('../')) {
                resolved = resolvePath(specifier, effectiveDirname);
                absolutePath = resolved;
                // Convert to file:// URL for registry lookup
                resolved = "file://" + resolved;
            } else if (specifier.startsWith('/')) {
                absolutePath = specifier;
                resolved = "file://" + specifier;
            }

            // Try to find module with extension resolution in registries
            var extensions = ['', '.js', '.mjs', '.cjs', '.json', '/index.js', '/index.mjs', '/index.cjs'];
            var found = null;

            for (var i = 0; i < extensions.length && !found; i++) {
                var tryPath = resolved + extensions[i];
                if (globalThis.__otter_cjs_modules[tryPath]) {
                    found = globalThis.__otter_cjs_modules[tryPath]();
                    break;
                }
                if (globalThis.__otter_modules && globalThis.__otter_modules[tryPath]) {
                    found = globalThis.__toCommonJS(globalThis.__otter_modules[tryPath]);
                    break;
                }
            }

            if (found) return found;

            // Try to load from file system
            var fs = globalThis.__otter_get_node_builtin ? globalThis.__otter_get_node_builtin('fs') : null;
            if (fs && absolutePath) {
                // Handle JSON files
                if (specifier.endsWith('.json')) {
                    if (__jsonCache[absolutePath]) {
                        return __jsonCache[absolutePath];
                    }
                    try {
                        var jsonContent = fs.readFileSync(absolutePath, 'utf8');
                        var jsonData = JSON.parse(jsonContent);
                        __jsonCache[absolutePath] = jsonData;
                        return jsonData;
                    } catch (e) {
                        // Fall through to error
                    }
                }

                // Handle JS/CJS files
                var resolvedPath = resolveFilePath(absolutePath, fs);
                if (resolvedPath) {
                    var loadedModule = loadFileModule(resolvedPath, fs);
                    if (loadedModule !== null) {
                        return loadedModule;
                    }
                }
            }

            throw new Error("Cannot find module '" + specifier + "' from '" + effectiveDirname + "'");
        };

        require.resolve = function(specifier) {
            // Resolve relative paths
            if (specifier.startsWith('./') || specifier.startsWith('../')) {
                var resolved = resolvePath(specifier, effectiveDirname);
                var fs = globalThis.__otter_get_node_builtin ? globalThis.__otter_get_node_builtin('fs') : null;
                if (fs) {
                    var resolvedPath = resolveFilePath(resolved, fs);
                    if (resolvedPath) return resolvedPath;
                }
                return resolved;
            }
            return specifier;
        };

        require.cache = __fileModuleCache;
        require.main = undefined;

        return require;
    };

    // Create global require for standalone scripts (not bundled)
    // Note: This will be overwritten when the entry script runs with proper dirname
    if (typeof globalThis.require !== "function") {
        globalThis.require = globalThis.__createRequire(".", "script.js");
    }

    // Expose helper to update global require with proper dirname
    globalThis.__otter_set_entry_dirname = function(dirname, filename) {
        globalThis.require = globalThis.__createRequire(dirname, filename);
        // Also set __dirname and __filename globals for the entry script
        globalThis.__dirname = dirname;
        globalThis.__filename = filename;
    };

})(globalThis);
