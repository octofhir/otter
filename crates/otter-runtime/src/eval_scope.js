'use strict';
// The scope a `-e`/`-p` snippet runs in. Node evaluates the snippet as the
// `[eval]` CommonJS module and publishes that module's own scope on the
// global object, plus a lazy global per builtin module, so a snippet can
// say `http.maxHeaderSize` without requiring anything.

const globals = globalThis;
const define = (name, value) => {
  Object.defineProperty(globals, name, {
    value,
    writable: true,
    enumerable: false,
    configurable: true,
  });
};

define('require', require);
define('module', module);
define('exports', exports);
// Node reports the snippet's name, not the path its module resolves from.
define('__filename', '[eval]');
define('__dirname', __dirname);

for (const name of require('module').builtinModules) {
  if (name.startsWith('_') || name.includes('/') || name.includes(':')) continue;
  if (Object.hasOwn(globals, name)) continue;
  Object.defineProperty(globals, name, {
    enumerable: false,
    configurable: true,
    get() {
      const value = require(name);
      define(name, value);
      return value;
    },
    set(value) {
      define(name, value);
    },
  });
}
