'use strict';
module.exports = {
  tryGetCwd() {
    try { return process.cwd(); } catch { return undefined; }
  },
  evalModuleEntryPoint() {},
};
