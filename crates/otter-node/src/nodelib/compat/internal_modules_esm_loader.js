'use strict';

// The ESM loader the test runner reaches for whenever it has to bring a
// module in by URL: a `--test-global-setup` file, a `--import` preload, a
// reporter named on the command line, a test file under `--test-isolation=none`.
//
// Bringing a module in by URL is what dynamic `import()` already does, so the
// loader is that, with the specifier resolved against its parent first —
// `import()` resolves against the module doing the importing, which here is
// this file rather than the caller's.
//
// Mocking a module means intercepting resolution and loading, which this
// runtime does not let a program do. That is `mock.module()`'s own path
// through `internal/modules/customization_hooks`, and it says so there.

let loader;

function absoluteURL(specifier, parentURL) {
  if (typeof specifier !== 'string') {
    // A `URL` — already absolute, and its own answer for `href`.
    return `${specifier}`;
  }
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(specifier)) {
    return specifier;
  }
  if (parentURL && (specifier[0] === '.' || specifier[0] === '/')) {
    return new URL(specifier, `${parentURL}`).href;
  }
  // A bare specifier: a package or a builtin, which resolves by name.
  return specifier;
}

function getOrInitializeCascadedLoader() {
  loader ??= {
    import(specifier, parentURL, importAttributes) {
      return import(absoluteURL(specifier, parentURL));
    },
  };
  return loader;
}

module.exports = { getOrInitializeCascadedLoader };
