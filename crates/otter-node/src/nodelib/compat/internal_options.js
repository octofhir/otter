'use strict';
// CLI option table read by vendored files. The switches this run carries are
// in `process.execArgv` — the same list a re-exec reproduces — so the table
// is the defaults with those parsed over the top.
const defaults = new Map([
  ['--stack-trace-limit', 10],
  ['--no-warnings', false],
  ['--pending-deprecation', false],
  ['--throw-deprecation', false],
  ['--trace-deprecation', false],
  ['--trace-warnings', false],
  ['--disable-proto', ''],
  ['--frozen-intrinsics', false],
  ['--max-http-header-size', 16384],
  ['--insecure-http-parser', false],
  ['--network-family-autoselection', true],
  ['--network-family-autoselection-attempt-timeout', 500],
]);

const values = new Map(defaults);

for (const argument of (typeof process === 'object' && process?.execArgv) || []) {
  const equals = argument.indexOf('=');
  const name = (equals === -1 ? argument : argument.slice(0, equals)).replaceAll('_', '-');
  if (equals === -1) {
    // A boolean switch, in either polarity: `--no-x` clears `--x`.
    if (defaults.has(name)) {
      values.set(name, true);
    } else if (name.startsWith('--no-') && defaults.has(`--${name.slice(5)}`)) {
      values.set(`--${name.slice(5)}`, false);
    }
    continue;
  }
  if (!defaults.has(name)) continue;
  const text = argument.slice(equals + 1);
  values.set(name, typeof defaults.get(name) === 'number' ? Number(text) : text);
}

module.exports = {
  getOptionValue(name) { return values.get(name); },
  options: new Map(),
  aliases: new Map(),
};
