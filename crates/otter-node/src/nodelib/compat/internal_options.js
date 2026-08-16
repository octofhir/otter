'use strict';
// CLI option table read by vendored files; every entry answers the default.
const defaults = new Map([
  ['--stack-trace-limit', 10],
  ['--no-warnings', false],
  ['--pending-deprecation', false],
  ['--throw-deprecation', false],
  ['--trace-deprecation', false],
  ['--trace-warnings', false],
  ['--disable-proto', ''],
  ['--frozen-intrinsics', false],
]);
module.exports = {
  getOptionValue(name) { return defaults.get(name); },
  options: new Map(),
  aliases: new Map(),
};
