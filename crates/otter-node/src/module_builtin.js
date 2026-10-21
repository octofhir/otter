'use strict';
// `node:module` — builtin-module metadata + a minimal Module class.
const { createRequire: requireAt } = require('internal/otter/natives');
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

// The `require` of a user module at `filename`, made by the loader itself:
// Node's resolution from that file's directory, the one module cache, and
// `filename` heading the require stack of whatever it cannot find. It is not
// this shim's own `require`, which, being a builtin's, reaches the internal
// modules no user module may load.
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
  let from = String(filename);
  if (isURL || from.startsWith('file:')) from = require('url').fileURLToPath(filename);
  // A directory argument stands for a file inside it, as Node's does.
  const proxy = from.endsWith('/') ? path.join(from, 'noop.js') : path.resolve(from);
  const fn = requireAt(proxy);
  fn.extensions = Module._extensions;
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
// `-r`/`--require`: each request is loaded before the entry runs, resolved as
// if required from a file in the working directory.
Module._preloadModules = (requests) => {
  if (!requests || requests.length === 0) return;
  const path = require('path');
  const preloadRequire = createRequire(path.join(process.cwd(), 'x'));
  for (const request of requests) preloadRequire(request);
};

module.exports = Module;
module.exports.Module = Module;
module.exports.builtinModules = builtinModules;
module.exports.isBuiltin = isBuiltin;
module.exports.createRequire = createRequire;
module.exports.constants = { compileCacheStatus: {} };
