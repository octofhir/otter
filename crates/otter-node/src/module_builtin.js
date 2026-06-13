'use strict';
// `node:module` — builtin-module metadata + a minimal Module class.
const builtinModules = [
  'assert', 'buffer', 'child_process', 'cluster', 'console', 'constants', 'crypto',
  'dgram', 'diagnostics_channel', 'dns', 'domain', 'events', 'fs', 'http', 'http2',
  'https', 'module', 'net', 'os', 'path', 'perf_hooks', 'process', 'punycode',
  'querystring', 'readline', 'repl', 'stream', 'string_decoder', 'timers', 'tls',
  'tty', 'url', 'util', 'v8', 'vm', 'worker_threads', 'zlib',
];

function isBuiltin(name) {
  return builtinModules.includes(String(name).replace(/^node:/, ''));
}

function createRequire() {
  const fn = function require() {
    const err = new Error('createRequire is not supported in this runtime');
    err.code = 'ERR_UNSUPPORTED';
    throw err;
  };
  fn.resolve = (id) => id;
  fn.resolve.paths = () => [];
  fn.cache = {};
  fn.extensions = {};
  fn.main = undefined;
  return fn;
}

class Module {
  constructor(id = '', parent) {
    this.id = id;
    this.path = '';
    this.exports = {};
    this.parent = parent;
    this.filename = null;
    this.loaded = false;
    this.children = [];
    this.paths = [];
  }
}
Module.builtinModules = builtinModules;
Module.isBuiltin = isBuiltin;
Module.createRequire = createRequire;
Module._cache = { __proto__: null };
Module._pathCache = { __proto__: null };
Module._extensions = { __proto__: null };
Module.globalPaths = [];
Module.syncBuiltinESMExports = () => {};
Module._nodeModulePaths = () => [];
Module._resolveLookupPaths = () => [];
Module.wrap = (script) => `(function (exports, require, module, __filename, __dirname) { ${script}\n});`;
Module.wrapper = ['(function (exports, require, module, __filename, __dirname) { ', '\n});'];
Module.setSourceMapsSupport = () => {};
Module.getSourceMapsSupport = () => ({ enabled: false });
Module.findSourceMap = () => undefined;
Module.register = () => {};

module.exports = Module;
module.exports.Module = Module;
module.exports.builtinModules = builtinModules;
module.exports.isBuiltin = isBuiltin;
module.exports.createRequire = createRequire;
module.exports.constants = { compileCacheStatus: {} };
