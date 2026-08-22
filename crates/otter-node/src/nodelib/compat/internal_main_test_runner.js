'use strict';

// Node's `--test` entry point, over a realm this runtime has already brought
// up.
//
// Node's own main prepares the execution environment before it starts the
// runner: it patches `process`, wires the module loaders, installs the global
// console. This runtime does all of that before any program runs, so what is
// left of that preparation for a test run is the `--require` preload, which
// Node performs here rather than at startup so a module a test file needs is
// in place before the runner takes over. Under `--test-isolation=none` the
// runner loads the user modules itself, in the scope of the root test, and the
// preload waits for it.

const { getOptionValue } = require('internal/options');
const { run } = require('internal/test_runner/runner');
const { parseCommandLine } = require('internal/test_runner/utils');

const options = parseCommandLine();

if (options.isolation !== 'none') {
  const { Module } = require('internal/modules/cjs/loader');
  Module._preloadModules(getOptionValue('--require'));
}

// Everything after the program name names what to test; naming nothing leaves
// the runner to discover the files itself.
options.globPatterns = process.argv.slice(1);

run(options).on('test:summary', (data) => {
  if (!data.success) {
    process.exitCode = 1;
  }
});
