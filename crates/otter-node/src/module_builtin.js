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

// `require` here is this shim's own resolver. Bare specifiers reach it
// unchanged; a relative one is resolved against `filename` first, which is the
// whole point of createRequire.
function createRequire(filename) {
  // `URL` is a web global; an embedder can run node modules without it.
  const isURL = typeof URL !== 'undefined' && filename instanceof URL;
  if (typeof filename !== 'string' && !isURL) {
    const err = new TypeError(
      'The "filename" argument must be of type string or an instance of URL. Received ' +
        (filename === null ? 'null' : typeof filename));
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  const path = require('path');
  let from = isURL ? filename.pathname : String(filename);
  if (from.startsWith('file://')) from = from.slice('file://'.length);
  // A directory argument names the base directly; anything else is a file whose
  // directory is the base.
  const base = from.endsWith('/') ? from : path.dirname(from);

  // Named `requireFrom`, not `require`: a named function expression binds its
  // own name in scope, which would shadow this shim's resolver and recurse.
  const fn = function requireFrom(id) {
    const spec = String(id);
    if (spec.startsWith('./') || spec.startsWith('../') || spec === '.' || spec === '..') {
      return require(path.resolve(base, spec));
    }
    return require(spec);
  };
  fn.resolve = (id) => {
    const spec = String(id);
    if (spec.startsWith('./') || spec.startsWith('../')) return path.resolve(base, spec);
    return spec;
  };
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
