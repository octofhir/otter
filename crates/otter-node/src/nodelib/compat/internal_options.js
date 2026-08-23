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
  ['--experimental-stream-iter', false],
  ['--network-family-autoselection', true],
  ['--network-family-autoselection-attempt-timeout', 500],
  // The test runner's switches, with the values Node gives them when the
  // switch is absent. A list-valued switch answers an empty list, never
  // nothing: the runner counts them without checking first.
  ['--test', false],
  ['--test-concurrency', 0],
  ['--test-force-exit', false],
  ['--test-global-setup', undefined],
  ['--test-isolation', 'process'],
  ['--test-name-pattern', []],
  ['--test-only', false],
  ['--test-random-seed', 0],
  ['--test-randomize', false],
  ['--test-reporter', []],
  ['--test-reporter-destination', []],
  ['--test-rerun-failures', undefined],
  ['--test-shard', undefined],
  ['--test-skip-pattern', []],
  ['--test-timeout', 0],
  ['--test-update-snapshots', false],
  ['--test-coverage-branches', 0],
  ['--test-coverage-exclude', []],
  ['--test-coverage-functions', 0],
  ['--test-coverage-include', []],
  ['--test-coverage-lines', 0],
  ['--experimental-test-coverage', false],
  ['--experimental-test-module-mocks', false],
  ['--experimental-test-tag-filter', []],
  ['--enable-source-maps', false],
  ['--import', []],
  ['--require', []],
  ['--strip-types', false],
  ['--watch', false],
  ['--unhandled-rejections', 'throw'],
]);

// Switches whose value is a list: repeating one adds to it.
const listValued = new Set([
  '--test-name-pattern',
  '--test-skip-pattern',
  '--test-reporter',
  '--test-reporter-destination',
  '--test-coverage-exclude',
  '--test-coverage-include',
  '--experimental-test-tag-filter',
  '--import',
  '--require',
]);

// The table, read off `process.execArgv`. Refreshing it re-reads that list,
// which is where a switch this run carries is written down.
const values = new Map();

function refreshOptions() {
  values.clear();
  for (const [name, value] of defaults) {
    // A fresh list per switch, so appending to one never shows up in another's.
    values.set(name, Array.isArray(value) ? [] : value);
  }
  // Whether a seed was given at all, which the runner asks about separately.
  values.set('[has_test_random_seed]', false);

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
    if (listValued.has(name)) {
      values.get(name).push(text);
    } else {
      values.set(name, typeof defaults.get(name) === 'number' ? Number(text) : text);
    }
    if (name === '--test-random-seed') values.set('[has_test_random_seed]', true);
  }
}

refreshOptions();

module.exports = {
  getOptionValue(name) { return values.get(name); },
  refreshOptions,
  // The switches this process was started with, as a list — what a child
  // process has to be started with to run the same way. That list is
  // `process.execArgv`.
  getOptionsAsFlagsFromBinding() {
    return [...((typeof process === 'object' && process?.execArgv) || [])];
  },
  getEmbedderOptions() {
    return { shouldNotRegisterESMLoader: false, noGlobalSearchPaths: false, hasEmbedderPreload: false };
  },
  options: new Map(),
  aliases: new Map(),
};
