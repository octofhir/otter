/**
 * Node.js module compatibility layer.
 *
 * Provides createRequire and other module-related APIs.
 */

(function() {
  // Module._cache equivalent
  const moduleCache = new Map();

  // createRequire creates a require function that can be used in ESM
  function createRequire(filename) {
    // Convert URL to path if needed
    let basePath = filename;
    if (typeof filename === 'object' && filename.href) {
      basePath = filename.href;
    }
    if (basePath.startsWith('file://')) {
      basePath = basePath.slice(7);
    }

    // Return a require-like function
    function require(specifier) {
      // Check if it's a node builtin
      if (specifier.startsWith('node:')) {
        const moduleName = specifier.slice(5);
        const builtin = globalThis.__otter_get_node_builtin(specifier);
        if (builtin) return builtin;
        throw new Error(`Cannot find module '${specifier}'`);
      }

      // Check if it's a bare node builtin (no node: prefix)
      const builtinCheck = globalThis.__otter_get_node_builtin('node:' + specifier);
      if (builtinCheck) return builtinCheck;

      // For relative/absolute paths, try to load
      throw new Error(`createRequire: Cannot resolve module '${specifier}' from '${basePath}'`);
    }

    require.resolve = function(specifier) {
      throw new Error(`require.resolve is not implemented: ${specifier}`);
    };

    require.cache = moduleCache;

    return require;
  }

  // Module class stub
  class Module {
    constructor(id, parent) {
      this.id = id;
      this.path = id;
      this.exports = {};
      this.parent = parent || null;
      this.filename = id;
      this.loaded = false;
      this.children = [];
      this.paths = [];
    }

    static createRequire = createRequire;
    static builtinModules = [
      'assert', 'buffer', 'child_process', 'cluster', 'console', 'constants',
      'crypto', 'dgram', 'dns', 'domain', 'events', 'fs', 'http', 'https',
      'module', 'net', 'os', 'path', 'perf_hooks', 'process', 'punycode',
      'querystring', 'readline', 'repl', 'stream', 'string_decoder', 'sys',
      'timers', 'tls', 'tty', 'url', 'util', 'v8', 'vm', 'worker_threads', 'zlib'
    ];
    static _cache = moduleCache;
    static _pathCache = {};
    static _extensions = {
      '.js': function(module, filename) {},
      '.json': function(module, filename) {},
      '.node': function(module, filename) {},
    };
    static globalPaths = [];
    static wrapper = [
      '(function (exports, require, module, __filename, __dirname) { ',
      '\n});'
    ];

    static wrap(script) {
      return Module.wrapper[0] + script + Module.wrapper[1];
    }

    static isBuiltin(moduleName) {
      const name = moduleName.startsWith('node:') ? moduleName.slice(5) : moduleName;
      return Module.builtinModules.includes(name);
    }

    static findSourceMap(path) {
      return undefined;
    }

    static syncBuiltinESMExports() {
      // No-op
    }
  }

  // syncBuiltinESMExports function
  function syncBuiltinESMExports() {
    // No-op stub
  }

  // findSourceMap function
  function findSourceMap(path) {
    return undefined;
  }

  // SourceMap class stub
  class SourceMap {
    constructor(payload) {
      this.payload = payload;
    }

    get payload() {
      return this._payload;
    }

    set payload(value) {
      this._payload = value;
    }

    findEntry(line, column) {
      return null;
    }
  }

  // isBuiltin function
  function isBuiltin(moduleName) {
    return Module.isBuiltin(moduleName);
  }

  const moduleModule = {
    Module,
    createRequire,
    builtinModules: Module.builtinModules,
    isBuiltin,
    syncBuiltinESMExports,
    findSourceMap,
    SourceMap,
    // Default export is Module itself for CJS compatibility
    default: Module,
  };

  // Register as node:module
  if (typeof globalThis.__registerNodeBuiltin === 'function') {
    globalThis.__registerNodeBuiltin('module', moduleModule);
  }
})();
